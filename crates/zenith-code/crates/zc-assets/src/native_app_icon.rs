//! `NativeAppIconResolver.ts`: a macOS application's icon as a cached 64×64 PNG, without
//! exposing host paths to clients.
//!
//! The application is found with Spotlight (`mdfind` by bundle id or display name, the most
//! recently used match by `mdls kMDItemLastUsedDate`), its `Info.plist` read with the `plist`
//! crate (TS: `plutil`), and its `.icns` decoded with the `icns` crate and scaled here (TS:
//! `sips`). The PNG lands in `<caches>/native-app-icons/<sha256>.png`, keyed by the app path, its
//! version and the icon path. Answers (including "no icon") are cached for an hour, 256 apps at
//! most; failures are not cached. Two resolutions run at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use zc_contracts::ToolActivityNativeAppReference;
use zc_core::cache::TtlCache;

use crate::preview::{basename, extname};

const ICON_SIZE: u32 = 64;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const RESOLUTION_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const RESOLUTION_CACHE_MAX_ENTRIES: usize = 256;

/// Runs a host command and returns its stdout (injectable: the TS test records the commands).
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn output(&self, command: &str, args: &[String]) -> Result<String, String>;
}

/// [`CommandRunner`] over the process runner (stdin and stderr ignored, 5 second timeout).
#[derive(Debug, Default)]
pub struct HostCommandRunner;

#[async_trait]
impl CommandRunner for HostCommandRunner {
    async fn output(&self, command: &str, args: &[String]) -> Result<String, String> {
        let mut input = zc_core::ProcessRunInput::new(command, args.iter().cloned());
        input.timeout = Some(COMMAND_TIMEOUT);
        match zc_core::run_process(input).await {
            Ok(output) if output.code == Some(0) && !output.timed_out => Ok(output.stdout),
            Ok(output) => Err(format!("{command} exited with {:?}", output.code)),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// `escapeSpotlightString`.
pub fn escape_spotlight_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '\'' | '*' | '?') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn contains_control_character(value: &str) -> bool {
    value.chars().any(|c| (c as u32) <= 31 || c as u32 == 127)
}

/// `appCacheKey`: the reference as JSON.
fn app_cache_key(app: &ToolActivityNativeAppReference) -> String {
    serde_json::to_string(app).unwrap_or_default()
}

/// `NativeAppIconResolver`.
#[derive(Clone)]
pub struct NativeAppIconResolver {
    cache_directory: PathBuf,
    commands: Arc<dyn CommandRunner>,
    darwin: bool,
    cache: TtlCache<String, Option<String>>,
    semaphore: Arc<Semaphore>,
}

impl std::fmt::Debug for NativeAppIconResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAppIconResolver")
            .field("cache_directory", &self.cache_directory)
            .finish_non_exhaustive()
    }
}

impl NativeAppIconResolver {
    /// `provider_status_cache_dir` is `<baseDir>/caches` (`providerStatusCacheDir`).
    pub fn new(provider_status_cache_dir: &Path) -> Self {
        Self::with_commands(provider_status_cache_dir, Arc::new(HostCommandRunner), cfg!(target_os = "macos"))
    }

    pub fn with_commands(provider_status_cache_dir: &Path, commands: Arc<dyn CommandRunner>, darwin: bool) -> Self {
        Self {
            cache_directory: provider_status_cache_dir.join("native-app-icons"),
            commands,
            darwin,
            cache: TtlCache::new(RESOLUTION_CACHE_MAX_ENTRIES, RESOLUTION_CACHE_TTL),
            semaphore: Arc::new(Semaphore::new(2)),
        }
    }

    /// `resolve`: a cached PNG path for the application, or `None`.
    pub async fn resolve(&self, app: &ToolActivityNativeAppReference) -> Option<String> {
        if !self.darwin {
            return None;
        }
        if let ToolActivityNativeAppReference::DisplayName(app) = app {
            if contains_control_character(&app.display_name) {
                return None;
            }
        }
        let key = app_cache_key(app);
        let cached = match self.cached(&key, app).await {
            Ok(cached) => cached?,
            Err(error) => {
                tracing::debug!(?app, %error, "Failed to resolve native application icon.");
                return None;
            }
        };
        if std::fs::metadata(&cached).is_ok_and(|metadata| metadata.is_file()) {
            return Some(cached);
        }
        self.cache.invalidate(&key);
        match self.cached(&key, app).await {
            Ok(cached) => cached,
            Err(error) => {
                tracing::debug!(?app, %error, "Failed to resolve native application icon.");
                None
            }
        }
    }

    async fn cached(&self, key: &String, app: &ToolActivityNativeAppReference) -> Result<Option<String>, String> {
        if let Some(cached) = self.cache.get(key) {
            return Ok(cached);
        }
        let resolved = {
            let _permit = self.semaphore.acquire().await.map_err(|error| error.to_string())?;
            self.resolve_uncached(app).await?
        };
        self.cache.insert(key.clone(), resolved.clone());
        Ok(resolved)
    }

    async fn plist_value(info_plist_path: &Path, key: &str) -> String {
        let path = info_plist_path.to_owned();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || {
            plist::Value::from_file(&path)
                .ok()
                .and_then(|value| value.into_dictionary())
                .and_then(|dictionary| dictionary.get(&key).cloned())
                .map(|value| match value {
                    plist::Value::String(text) => text,
                    plist::Value::Integer(number) => number.to_string(),
                    plist::Value::Real(number) => number.to_string(),
                    plist::Value::Boolean(flag) => flag.to_string(),
                    _ => String::new(),
                })
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .await
        .unwrap_or_default()
    }

    /// `resolveApplicationPath`.
    async fn resolve_application_path(&self, app: &ToolActivityNativeAppReference) -> Result<Option<String>, String> {
        let query = match app {
            ToolActivityNativeAppReference::AppId(app) => format!("kMDItemCFBundleIdentifier == '{}'", app.app_id),
            ToolActivityNativeAppReference::DisplayName(app) => format!(
                "kMDItemContentType == 'com.apple.application-bundle' && kMDItemDisplayName == '{}'",
                escape_spotlight_string(&app.display_name)
            ),
        };
        let spotlight = self.commands.output("/usr/bin/mdfind", &[query]).await?;
        let candidates: Vec<String> = spotlight
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(".app"))
            .map(str::to_owned)
            .collect();
        let matching: Vec<String> = match app {
            ToolActivityNativeAppReference::AppId(_) => candidates.clone(),
            ToolActivityNativeAppReference::DisplayName(app) => candidates
                .iter()
                .filter(|candidate| {
                    let name = basename(candidate);
                    let stem = name.strip_suffix(".app").unwrap_or(&name);
                    stem.to_lowercase() == app.display_name.to_lowercase()
                })
                .cloned()
                .collect(),
        };
        let ranked = if matching.is_empty() { candidates } else { matching };
        let mut most_recent: Option<(String, String)> = None;
        for candidate in ranked {
            let last_used = self
                .commands
                .output(
                    "/usr/bin/mdls",
                    &["-raw".into(), "-name".into(), "kMDItemLastUsedDate".into(), candidate.clone()],
                )
                .await
                .map(|value| value.trim().to_owned())
                .unwrap_or_default();
            if most_recent.as_ref().is_none_or(|(_, best)| last_used > *best) {
                most_recent = Some((candidate, last_used));
            }
        }
        Ok(most_recent.map(|(path, _)| path))
    }

    /// `resolveNativeAppIconUncached`.
    async fn resolve_uncached(&self, app: &ToolActivityNativeAppReference) -> Result<Option<String>, String> {
        let Some(app_path) = self.resolve_application_path(app).await? else {
            return Ok(None);
        };
        let canonical_app_path = std::fs::canonicalize(&app_path).map_err(|error| error.to_string())?;
        let info_plist_path = canonical_app_path.join("Contents").join("Info.plist");
        let resources_directory = canonical_app_path.join("Contents").join("Resources");
        let mut icon_name = Self::plist_value(&info_plist_path, "CFBundleIconFile").await;
        if icon_name.is_empty() {
            icon_name = Self::plist_value(&info_plist_path, "CFBundleIconName").await;
        }
        if !icon_name.is_empty() && basename(&icon_name) != icon_name {
            return Ok(None);
        }
        let icon_file_name = (!icon_name.is_empty()).then(|| {
            if extname(&icon_name).is_empty() {
                format!("{icon_name}.icns")
            } else {
                icon_name.clone()
            }
        });
        let existing_file = |path: PathBuf| std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()).then_some(path);
        let first_icns = std::fs::read_dir(&resources_directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .find(|name| name.to_lowercase().ends_with(".icns"))
            })
            .ok()
            .flatten();
        let source_icon_candidate = icon_file_name
            .and_then(|name| existing_file(resources_directory.join(name)))
            .or_else(|| existing_file(resources_directory.join("AppIcon.icns")))
            .or_else(|| first_icns.and_then(|name| existing_file(resources_directory.join(name))));
        let Some(source_icon_candidate) = source_icon_candidate else {
            return Ok(None);
        };
        let source_icon_path = std::fs::canonicalize(&source_icon_candidate).map_err(|error| error.to_string())?;
        if !source_icon_path.starts_with(&resources_directory) || source_icon_path == resources_directory {
            return Ok(None);
        }

        let mut app_version = Self::plist_value(&info_plist_path, "CFBundleVersion").await;
        if app_version.is_empty() {
            app_version = Self::plist_value(&info_plist_path, "CFBundleShortVersionString").await;
        }
        let digest = Sha256::digest(format!("{}\0{app_version}\0{}", canonical_app_path.display(), source_icon_path.display()).as_bytes());
        let cache_key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let cache_path = self.cache_directory.join(format!("{cache_key}.png"));
        if cache_path.is_file() {
            return Ok(Some(cache_path.to_string_lossy().into_owned()));
        }
        let cache_directory = self.cache_directory.clone();
        let temporary_path = cache_directory.join(format!(".{cache_key}-{}-{}.png", std::process::id(), zc_core::uuid_v4()));
        let destination = cache_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            std::fs::create_dir_all(&cache_directory).map_err(|error| error.to_string())?;
            let result = convert_icns_to_png(&source_icon_path, &temporary_path, ICON_SIZE)
                .and_then(|()| std::fs::rename(&temporary_path, &destination).map_err(|error| error.to_string()));
            let _ = std::fs::remove_file(&temporary_path);
            result
        })
        .await
        .map_err(|error| error.to_string())??;
        Ok(cache_path.is_file().then(|| cache_path.to_string_lossy().into_owned()))
    }
}

/// The icon of `icns_path` as a `size`×`size` PNG at `png_path` (TS: `sips -z 64 64 -s format
/// png`): the smallest image at least `size` wide, else the largest, scaled with an
/// area-averaging (premultiplied alpha) filter.
pub fn convert_icns_to_png(icns_path: &Path, png_path: &Path, size: u32) -> Result<(), String> {
    let file = std::fs::File::open(icns_path).map_err(|error| error.to_string())?;
    let family = icns::IconFamily::read(std::io::BufReader::new(file)).map_err(|error| error.to_string())?;
    let mut types = family.available_icons();
    types.sort_by_key(|icon_type| {
        let width = icon_type.pixel_width();
        if width >= size {
            (0, width)
        } else {
            (1, u32::MAX - width)
        }
    });
    let image = types
        .into_iter()
        .find_map(|icon_type| family.get_icon_with_type(icon_type).ok())
        .ok_or_else(|| format!("no decodable icon in {}", icns_path.display()))?;
    let rgba = image.convert_to(icns::PixelFormat::RGBA);
    let scaled = scale_rgba(rgba.data(), rgba.width(), rgba.height(), size, size);
    let output = icns::Image::from_data(icns::PixelFormat::RGBA, size, size, scaled).map_err(|error| error.to_string())?;
    let file = std::fs::File::create(png_path).map_err(|error| error.to_string())?;
    output.write_png(std::io::BufWriter::new(file)).map_err(|error| error.to_string())
}

/// Area-averaging resample of straight RGBA (premultiplied while averaging).
fn scale_rgba(source: &[u8], source_width: u32, source_height: u32, width: u32, height: u32) -> Vec<u8> {
    let (sw, sh) = (f64::from(source_width), f64::from(source_height));
    let mut out = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let y0 = f64::from(y) * sh / f64::from(height);
        let y1 = (f64::from(y + 1) * sh / f64::from(height)).max(y0 + 1e-9);
        for x in 0..width {
            let x0 = f64::from(x) * sw / f64::from(width);
            let x1 = (f64::from(x + 1) * sw / f64::from(width)).max(x0 + 1e-9);
            let (mut r, mut g, mut b, mut a, mut total) = (0f64, 0f64, 0f64, 0f64, 0f64);
            let mut sy = y0.floor();
            while sy < y1 {
                let wy = (y1.min(sy + 1.0) - y0.max(sy)).max(0.0);
                let mut sx = x0.floor();
                while sx < x1 {
                    let wx = (x1.min(sx + 1.0) - x0.max(sx)).max(0.0);
                    let weight = wx * wy;
                    let px = (sx as u32).min(source_width - 1);
                    let py = (sy as u32).min(source_height - 1);
                    let index = ((py * source_width + px) * 4) as usize;
                    let alpha = f64::from(source[index + 3]) / 255.0;
                    r += f64::from(source[index]) * alpha * weight;
                    g += f64::from(source[index + 1]) * alpha * weight;
                    b += f64::from(source[index + 2]) * alpha * weight;
                    a += alpha * weight;
                    total += weight;
                    sx += 1.0;
                }
                sy += 1.0;
            }
            let index = ((y * width + x) * 4) as usize;
            if a > 0.0 {
                out[index] = (r / a).round().clamp(0.0, 255.0) as u8;
                out[index + 1] = (g / a).round().clamp(0.0, 255.0) as u8;
                out[index + 2] = (b / a).round().clamp(0.0, 255.0) as u8;
            }
            out[index + 3] = if total > 0.0 {
                (a / total * 255.0).round().clamp(0.0, 255.0) as u8
            } else {
                0
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! `NativeAppIconResolver.test.ts`, plus the icon conversion.
    use std::sync::Mutex;

    use zc_contracts::{LitAppId, LitDisplayName, ToolActivityNativeAppReferenceAppId, ToolActivityNativeAppReferenceDisplayName};

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, Vec<String>)>>);

    #[async_trait]
    impl CommandRunner for Recorder {
        async fn output(&self, command: &str, args: &[String]) -> Result<String, String> {
            self.0.lock().unwrap().push((command.to_owned(), args.to_vec()));
            Ok(String::new())
        }
    }

    fn display_name(name: &str) -> ToolActivityNativeAppReference {
        ToolActivityNativeAppReference::DisplayName(ToolActivityNativeAppReferenceDisplayName {
            tag: LitDisplayName,
            display_name: name.into(),
        })
    }

    #[tokio::test]
    async fn escapes_spotlight_wildcards_and_caches_misses() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let resolver = NativeAppIconResolver::with_commands(dir.path(), recorder.clone(), true);
        let app = display_name("Review * App");
        assert_eq!(resolver.resolve(&app).await, None);
        assert_eq!(resolver.resolve(&app).await, None);
        {
            let commands = recorder.0.lock().unwrap();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].0, "/usr/bin/mdfind");
            assert!(commands[0].1[0].contains("Review \\* App"));
        }
        for index in 0..256 {
            assert_eq!(resolver.resolve(&display_name(&format!("Missing Review App {index}"))).await, None);
        }
        assert_eq!(recorder.0.lock().unwrap().len(), 257);
        assert_eq!(resolver.resolve(&app).await, None);
        assert_eq!(recorder.0.lock().unwrap().len(), 258);
    }

    #[tokio::test]
    async fn resolves_nothing_off_macos_or_for_control_characters() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let app = ToolActivityNativeAppReference::AppId(ToolActivityNativeAppReferenceAppId {
            tag: LitAppId,
            app_id: "com.example.Editor".into(),
        });
        assert_eq!(
            NativeAppIconResolver::with_commands(dir.path(), recorder.clone(), false).resolve(&app).await,
            None
        );
        let resolver = NativeAppIconResolver::with_commands(dir.path(), recorder.clone(), true);
        assert_eq!(resolver.resolve(&display_name("Bad\u{7}Name")).await, None);
        assert!(recorder.0.lock().unwrap().is_empty());
    }

    /// A fake application bundle, found by a fake Spotlight.
    struct FakeSpotlight(String);

    #[async_trait]
    impl CommandRunner for FakeSpotlight {
        async fn output(&self, command: &str, _args: &[String]) -> Result<String, String> {
            Ok(if command.ends_with("mdfind") {
                format!("{}\n", self.0)
            } else {
                "2026-09-30 10:00:00 +0000".into()
            })
        }
    }

    #[tokio::test]
    async fn converts_a_bundle_icon_into_a_cached_png() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Sample Editor.app");
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("CFBundleIconFile".into(), plist::Value::String("Editor".into()));
        dictionary.insert("CFBundleVersion".into(), plist::Value::String("42".into()));
        plist::Value::Dictionary(dictionary).to_file_xml(app.join("Contents/Info.plist")).unwrap();
        let mut family = icns::IconFamily::new();
        let pixels: Vec<u8> = (0..128 * 128).flat_map(|i| [(i % 256) as u8, 40, 200, 255]).collect();
        family
            .add_icon(&icns::Image::from_data(icns::PixelFormat::RGBA, 128, 128, pixels).unwrap())
            .unwrap();
        family.write(std::fs::File::create(resources.join("Editor.icns")).unwrap()).unwrap();

        let caches = dir.path().join("caches");
        let resolver = NativeAppIconResolver::with_commands(&caches, Arc::new(FakeSpotlight(app.to_string_lossy().into_owned())), true);
        let icon = resolver.resolve(&display_name("Sample Editor")).await.expect("an icon");
        assert!(icon.starts_with(&*caches.join("native-app-icons").to_string_lossy()));
        let decoded = icns::Image::read_png(std::fs::File::open(&icon).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        // Deleted from the cache directory: converted again on the next request.
        std::fs::remove_file(&icon).unwrap();
        assert_eq!(resolver.resolve(&display_name("Sample Editor")).await.as_deref(), Some(icon.as_str()));
    }
}
