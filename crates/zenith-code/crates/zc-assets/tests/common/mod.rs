//! A server base directory in a temp dir, and the asset services over it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use zc_assets::github_media::{MediaHttp, TokenSource, UpstreamResponse};
use zc_assets::{Assets, GitHubMediaFetch, NativeAppIconResolver, ProjectFaviconResolver};
use zc_auth::TestClock;
use zc_core::ServerSecretStore;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub base_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub secrets: ServerSecretStore,
    pub clock: Arc<TestClock>,
    pub assets: Arc<Assets>,
}

struct NoNetwork;

#[async_trait]
impl MediaHttp for NoNetwork {
    async fn get(&self, _url: &str, _headers: Vec<(String, String)>) -> Result<UpstreamResponse, String> {
        Err("no network in tests".into())
    }
}

struct NoToken;

#[async_trait]
impl TokenSource for NoToken {
    async fn token(&self, _cwd: &str, _host: &str) -> String {
        String::new()
    }
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with_favicons(ProjectFaviconResolver::default()).await
    }

    pub async fn with_favicons(favicons: ProjectFaviconResolver) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = std::fs::canonicalize(dir.path()).unwrap();
        let paths = zc_core::derive_server_paths(&base_dir, None, true);
        std::fs::create_dir_all(&paths.attachments_dir).unwrap();
        let secrets = ServerSecretStore::open(&paths.secrets_dir).await.unwrap();
        let clock = TestClock::new(zc_core::now_millis());
        let assets = Arc::new(Assets::with_parts(
            &paths.attachments_dir,
            secrets.clone(),
            clock.clone(),
            favicons,
            NativeAppIconResolver::with_commands(&paths.provider_status_cache_dir, Arc::new(NoCommands), false),
            GitHubMediaFetch::new(Arc::new(NoNetwork), Arc::new(NoToken), clock.clone()),
        ));
        Self {
            dir,
            base_dir,
            attachments_dir: paths.attachments_dir,
            secrets,
            clock,
            assets,
        }
    }

    /// A fresh directory under the base dir.
    pub fn mkdir(&self, name: &str) -> String {
        let path = self.base_dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().into_owned()
    }
}

struct NoCommands;

#[async_trait]
impl zc_assets::native_app_icon::CommandRunner for NoCommands {
    async fn output(&self, _command: &str, _args: &[String]) -> Result<String, String> {
        Ok(String::new())
    }
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

pub fn real(path: impl AsRef<Path>) -> String {
    std::fs::canonicalize(path).unwrap().to_string_lossy().into_owned()
}

/// `(token, name)` of a relative asset URL.
pub fn split_url(relative_url: &str) -> (String, String) {
    let suffix = relative_url.strip_prefix(&format!("{}/", zc_assets::ASSET_ROUTE_PREFIX)).unwrap();
    let separator = suffix.find('/').unwrap();
    (suffix[..separator].to_owned(), suffix[separator + 1..].to_owned())
}
