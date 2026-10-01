//! `textGeneration/ThreadTitleLinks.ts`: before titling a thread, look up (at most) the first
//! two change-request / issue links of the message on their forge, so the title names the
//! subject instead of "Review PR 123".

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use regex::Regex;
use serde_json::json;
use url::Url;

use crate::js::slice_head16;

/// `SourceControlLinkSubject`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSubject {
    pub title: String,
    pub body: Option<String>,
}

/// A pending lookup; `Err` carries the failure detail (only "unavailable" reaches the prompt).
pub type LinkLookupFuture = BoxFuture<'static, Result<LinkSubject, String>>;

/// `SourceControlProviderRegistry.resolveLink`: `None` (synchronously) for URLs no provider
/// reads, a lookup otherwise.
pub trait ThreadTitleLinkResolver: Send + Sync {
    fn resolve_link(&self, cwd: &str, url: &Url) -> Option<LinkLookupFuture>;
}

impl ThreadTitleLinkResolver for zc_sourcecontrol::registry::SourceControlProviderRegistry {
    fn resolve_link(&self, cwd: &str, url: &Url) -> Option<LinkLookupFuture> {
        let lookup = zc_sourcecontrol::registry::SourceControlProviderRegistry::resolve_link(self, cwd, url)?;
        Some(Box::pin(async move {
            lookup
                .await
                .map(|subject| LinkSubject {
                    title: subject.title,
                    body: subject.body,
                })
                .map_err(|error| error.detail)
        }))
    }
}

impl<T: ThreadTitleLinkResolver + ?Sized> ThreadTitleLinkResolver for Arc<T> {
    fn resolve_link(&self, cwd: &str, url: &Url) -> Option<LinkLookupFuture> {
        (**self).resolve_link(cwd, url)
    }
}

/// How long one lookup may take.
pub const LINK_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_LINKS: usize = 2;

static HTTPS_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://[^\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}<>"')\]`]+"#).expect("link pattern")
});
static TRAILING_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.,;!?]+$").expect("trailing punctuation"));

/// `resolveThreadTitleLinks({message, cwd})`: `<url>\n{"title":…,"body":…}` per resolved link
/// (title cut at 300, body at 1,200 characters), `<url>: unavailable` when a lookup fails or
/// takes over 3 seconds, joined by blank lines; `None` without supported links.
pub async fn resolve_thread_title_links(resolver: &dyn ThreadTitleLinkResolver, message: &str, cwd: &str) -> Option<String> {
    let mut links: Vec<(String, LinkLookupFuture)> = Vec::new();
    for found in HTTPS_LINK.find_iter(message) {
        let candidate = TRAILING_PUNCTUATION.replace(found.as_str(), "");
        let Ok(mut url) = Url::parse(&candidate) else { continue };
        url.set_fragment(None);
        url.set_query(None);
        let href = url.as_str().to_owned();
        if links.iter().any(|(known, _)| *known == href) {
            continue;
        }
        let Some(lookup) = resolver.resolve_link(cwd, &url) else { continue };
        links.push((href, lookup));
        if links.len() == MAX_LINKS {
            break;
        }
    }
    let subjects = futures::future::join_all(links.into_iter().map(|(url, lookup)| async move {
        match tokio::time::timeout(LINK_LOOKUP_TIMEOUT, lookup).await {
            Ok(Ok(subject)) => {
                let summary = json!({
                    "title": slice_head16(&subject.title, 300),
                    "body": subject.body.as_deref().map(|body| slice_head16(body, 1_200)).unwrap_or_default(),
                });
                format!("{url}\n{summary}")
            }
            _ => format!("{url}: unavailable"),
        }
    }))
    .await;
    (!subjects.is_empty()).then(|| subjects.join("\n\n"))
}
