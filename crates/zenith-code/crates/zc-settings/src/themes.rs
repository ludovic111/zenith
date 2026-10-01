//! Environment themes (`apps/server/src/environmentTheme.ts`): the palettes this machine
//! publishes in `<stateDir>/themes/<id>.json`, watched and streamed to clients.
//!
//! Theming is cosmetic, so every failure degrades to "not published". The directory is local,
//! not a trust boundary, but what it can cost is capped: 32 files examined, 32 KiB per file,
//! 192 KiB in total. Each file is read through one descriptor opened with `O_NOFOLLOW |
//! O_NONBLOCK` (a symlinked file is rejected, a FIFO cannot block), and the type and size checks
//! use that descriptor.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};
use regex::Regex;
use serde_json::{Map, Value};
use tokio::sync::watch;
use zc_contracts::EnvironmentTheme;

use crate::js::{js_equal, js_trim};
use crate::watch::{watch_directory, DirWatch, WATCH_DEBOUNCE};

const THEME_FILE_SUFFIX: &str = ".json";
/// Files examined per read (not themes accepted).
pub const MAX_THEME_FILES: usize = 32;
/// `MAX_THEME_FILE_BYTES`.
pub const MAX_THEME_FILE_BYTES: u64 = 32 * 1024;
/// Total bytes of accepted themes.
pub const MAX_THEME_TOTAL_BYTES: usize = 192 * 1024;

/// `UNPUBLISHABLE_THEME_IDS` (`packages/shared/src/themePalettes.ts`).
pub const UNPUBLISHABLE_THEME_IDS: &[&str] = &[
    "system",
    "light",
    "dark",
    "zenith",
    "t3-chat",
    "grove",
    "ocean",
    "ember",
    "iris",
    "t3-chat-dark",
    "t3-grove",
    "t3-ocean",
    "t3-ember",
    "t3-iris",
    "t3-code",
];

fn regex(cell: &'static std::sync::OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

/// `EnvironmentThemeId`: `^(?!(?:system|light|dark)$)[a-z0-9](?:[a-z0-9-]{0,47})$`.
pub fn is_environment_theme_id(id: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    !matches!(id, "system" | "light" | "dark") && regex(&RE, r"^[a-z0-9][a-z0-9-]{0,47}$").is_match(id)
}

fn is_theme_color(value: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    regex(&RE, r"^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$").is_match(value)
}

fn is_color_role(value: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    regex(&RE, r"^[a-zA-Z][a-zA-Z0-9]{0,63}$").is_match(value)
}

fn js_len(text: &str) -> usize {
    zc_core::defect::js_length(text)
}

/// `EnvironmentThemeColors`: role-shaped keys, trimmed non-empty values of at most 64.
fn decode_colors(value: &Value) -> Result<Value, String> {
    let Value::Object(map) = value else {
        return Err("Expected object".into());
    };
    let mut out = Map::new();
    for (key, color) in map {
        if !is_color_role(key) {
            return Err(format!("Invalid color role {key:?}"));
        }
        let Value::String(color) = color else {
            return Err("Expected string".into());
        };
        let trimmed = js_trim(color);
        if trimmed.is_empty() || js_len(trimmed) > 64 {
            return Err("Invalid color value".into());
        }
        out.insert(key.clone(), Value::String(trimmed.to_owned()));
    }
    Ok(Value::Object(out))
}

/// `Schema.decodeUnknown(Schema.fromJsonString(EnvironmentThemeFile))`, encoded back.
pub fn decode_theme_file(raw: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(raw).map_err(|_| "Expected a valid JSON string".to_owned())?;
    let Value::Object(map) = parsed else {
        return Err("Expected object".into());
    };
    let mut out = Map::new();
    match map.get("version") {
        None => {}
        Some(version) if version.as_f64() == Some(1.0) => {
            out.insert("version".into(), Value::from(1));
        }
        Some(_) => return Err("Expected 1 at [\"version\"]".into()),
    }
    let name = match map.get("name") {
        Some(Value::String(name)) => js_trim(name),
        Some(_) => return Err("Expected string at [\"name\"]".into()),
        None => return Err("Missing key at [\"name\"]".into()),
    };
    if name.is_empty() || js_len(name) > 48 {
        return Err("Invalid value at [\"name\"]".into());
    }
    out.insert("name".into(), Value::String(name.to_owned()));
    match map.get("appearance").and_then(Value::as_str) {
        Some(appearance @ ("light" | "dark")) => {
            out.insert("appearance".into(), Value::String(appearance.to_owned()));
        }
        _ => return Err("Expected \"light\" | \"dark\" at [\"appearance\"]".into()),
    }
    for key in ["canvas", "accent"] {
        match map.get(key) {
            None => {}
            Some(Value::String(color)) if is_theme_color(color) => {
                out.insert(key.into(), Value::String(color.clone()));
            }
            Some(_) => return Err(format!("Invalid value at [\"{key}\"]")),
        }
    }
    if let Some(colors) = map.get("colors") {
        out.insert("colors".into(), decode_colors(colors)?);
    }
    if let Some(variants) = map.get("variants") {
        let Value::Object(variants) = variants else {
            return Err("Expected object at [\"variants\"]".into());
        };
        let mut decoded = Map::new();
        for key in ["light", "dark"] {
            if let Some(colors) = variants.get(key) {
                decoded.insert(key.into(), decode_colors(colors)?);
            }
        }
        out.insert("variants".into(), Value::Object(decoded));
    }
    Ok(Value::Object(out))
}

/// `environmentThemeFileHasColors`: both seeds, or a non-empty palette.
pub fn environment_theme_file_has_colors(file: &Value) -> bool {
    (file.get("canvas").is_some() && file.get("accent").is_some()) || file.get("colors").and_then(Value::as_object).is_some_and(|colors| !colors.is_empty())
}

/// `readThemeFileGuarded`: the file's text, or `None` unless it is a regular file of at most
/// `max_bytes` (symlinks and FIFOs refused through the open flags).
pub fn read_theme_file_guarded(file_path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file_path)
        .ok()?;
    let info = file.metadata().ok()?;
    if !info.is_file() || info.len() > max_bytes {
        return None;
    }
    let mut contents = Vec::with_capacity(info.len() as usize);
    (&mut file).take(info.len()).read_to_end(&mut contents).ok()?;
    Some(String::from_utf8_lossy(&contents).into_owned())
}

/// `readPublishedThemes`: every theme the directory publishes, in file-name order; anything
/// missing, unreadable, malformed, colorless or misnamed is skipped. Encoded `EnvironmentTheme`s.
pub fn read_published_themes(themes_dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(themes_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut themes = Vec::new();
    let mut examined = 0;
    let mut total_bytes = 0;
    for entry in names {
        let Some(id) = entry.strip_suffix(THEME_FILE_SUFFIX) else {
            continue;
        };
        if !is_environment_theme_id(id) || UNPUBLISHABLE_THEME_IDS.contains(&id) {
            continue;
        }
        examined += 1;
        if examined > MAX_THEME_FILES {
            tracing::warn!(path = %themes_dir.display(), limit = MAX_THEME_FILES, "ignoring environment theme files past the limit");
            break;
        }
        let file_path = PathBuf::from(format!("{}/{entry}", themes_dir.display()));
        let Some(raw) = read_theme_file_guarded(&file_path, MAX_THEME_FILE_BYTES) else {
            tracing::warn!(path = %file_path.display(), limit = MAX_THEME_FILE_BYTES, "ignoring unusable environment theme file");
            continue;
        };
        if js_trim(&raw).is_empty() {
            continue;
        }
        let file = match decode_theme_file(&raw) {
            Ok(file) => file,
            Err(detail) => {
                tracing::warn!(path = %file_path.display(), %detail, "ignoring invalid environment theme");
                continue;
            }
        };
        if !environment_theme_file_has_colors(&file) {
            tracing::warn!(path = %file_path.display(), "ignoring environment theme without colors");
            continue;
        }
        total_bytes += raw.len();
        if total_bytes > MAX_THEME_TOTAL_BYTES {
            tracing::warn!(path = %themes_dir.display(), limit = MAX_THEME_TOTAL_BYTES, "ignoring environment themes past the total size limit");
            break;
        }
        let mut theme = Map::new();
        theme.insert("id".into(), Value::String(id.to_owned()));
        if let Value::Object(file) = file {
            theme.extend(file);
        }
        themes.push(Value::Object(theme));
    }
    themes
}

/// The published set with the sequence number it was observed at.
#[derive(Debug, Clone, PartialEq)]
struct PublishedThemes {
    seq: u64,
    themes: Vec<Value>,
}

struct Inner {
    themes_dir: PathBuf,
    /// Guards the whole read/compare/publish so refreshes cannot publish out of order.
    refresh_lock: tokio::sync::Mutex<()>,
    /// The latest published set; a `watch` channel is the sliding PubSub of capacity 1.
    published: watch::Sender<PublishedThemes>,
    watcher: Mutex<Option<DirWatch>>,
}

/// `EnvironmentThemeService`. Cheap to clone.
#[derive(Clone)]
pub struct EnvironmentThemeService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for EnvironmentThemeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentThemeService")
            .field("themes_dir", &self.inner.themes_dir)
            .finish_non_exhaustive()
    }
}

/// Encoded themes to the generated type (themes that do not convert are dropped).
pub fn to_typed_themes(themes: &[Value]) -> Vec<EnvironmentTheme> {
    themes.iter().filter_map(|theme| serde_json::from_value(theme.clone()).ok()).collect()
}

impl EnvironmentThemeService {
    /// `layer`: create the directory, read it once, then watch it (debounced).
    pub async fn start(themes_dir: impl Into<PathBuf>) -> Self {
        let themes_dir = themes_dir.into();
        let (published, _) = watch::channel(PublishedThemes { seq: 0, themes: Vec::new() });
        let service = Self {
            inner: Arc::new(Inner {
                themes_dir: themes_dir.clone(),
                refresh_lock: tokio::sync::Mutex::new(()),
                published,
                watcher: Mutex::new(None),
            }),
        };
        if let Err(error) = tokio::fs::create_dir_all(&themes_dir).await {
            tracing::warn!(%error, path = %themes_dir.display(), "could not create the themes directory");
        }
        service.refresh().await;
        let refresher = service.clone();
        let handler: crate::watch::ChangeHandler = Arc::new(move || {
            let refresher = refresher.clone();
            Box::pin(async move {
                refresher.refresh().await;
            })
        });
        match watch_directory(&themes_dir, |_| true, WATCH_DEBOUNCE, handler) {
            Ok(watch) => *service.inner.watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(watch),
            Err(error) => tracing::warn!(%error, "could not watch the themes directory"),
        }
        service
    }

    /// Read the directory and publish when the set changed (structural comparison).
    async fn refresh(&self) -> PublishedThemes {
        let _permit = self.inner.refresh_lock.lock().await;
        let dir = self.inner.themes_dir.clone();
        let themes = tokio::task::spawn_blocking(move || read_published_themes(&dir)).await.unwrap_or_default();
        let mut next = None;
        self.inner.published.send_if_modified(|current| {
            let same = current.themes.len() == themes.len() && current.themes.iter().zip(&themes).all(|(a, b)| js_equal(a, b));
            if same {
                return false;
            }
            current.seq += 1;
            current.themes = themes.clone();
            next = Some(current.clone());
            true
        });
        next.unwrap_or_else(|| self.inner.published.borrow().clone())
    }

    /// `current`: what the directory publishes right now (read from disk).
    pub async fn current(&self) -> Vec<Value> {
        self.refresh().await.themes
    }

    /// `streamChanges`: the current set, then every later set, never one older than the first.
    pub async fn stream_changes(&self) -> BoxStream<'static, Vec<Value>> {
        let mut receiver = self.inner.published.subscribe();
        receiver.mark_unchanged();
        let snapshot = self.refresh().await;
        let after = snapshot.seq;
        let updates = stream::unfold(receiver, move |mut receiver| async move {
            loop {
                receiver.changed().await.ok()?;
                let update = receiver.borrow_and_update().clone();
                if update.seq > after {
                    return Some((update.themes, receiver));
                }
            }
        });
        stream::once(async move { snapshot.themes }).chain(updates).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_ids_follow_the_client_rule() {
        assert!(is_environment_theme_id("nord"));
        assert!(is_environment_theme_id("solarized-2"));
        assert!(!is_environment_theme_id("Nord"));
        assert!(!is_environment_theme_id("dark"));
        assert!(!is_environment_theme_id("-x"));
        assert!(!is_environment_theme_id(&"a".repeat(49)));
    }

    #[test]
    fn decodes_theme_files() {
        let file = decode_theme_file(r##"{"id":"ignored","name":" Nord ","appearance":"dark","canvas":"#2e3440","accent":"#88c0d0"}"##).unwrap();
        assert_eq!(
            file,
            serde_json::json!({"name": "Nord", "appearance": "dark", "canvas": "#2e3440", "accent": "#88c0d0"})
        );
        assert!(environment_theme_file_has_colors(&file));
        assert!(decode_theme_file(r#"{"name":"x","appearance":"sepia"}"#).is_err());
        assert!(decode_theme_file(r#"{"name":"x","appearance":"dark","canvas":"red"}"#).is_err());
        assert!(decode_theme_file("{").is_err());
        let colorless = decode_theme_file(r#"{"name":"x","appearance":"dark","colors":{}}"#).unwrap();
        assert!(!environment_theme_file_has_colors(&colorless));
    }
}
