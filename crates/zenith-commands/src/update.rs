//! Updates from GitHub Releases of `ludovic111/zenith` (lsuite standard §3).
//!
//! `.github/workflows/release.yml` publishes a `vX.Y.Z` tag with one asset per platform under
//! a stable name (`zenith-macos-arm64.zip`, `zenith-macos-x86_64.zip`,
//! `zenith-linux-x86_64.tar.gz`), a `SHA256SUMS` file and its Ed25519 signature
//! `SHA256SUMS.sig` (made with the secret whose public half is `assets/update-signing.pub`).
//!
//! The app compares the latest tag with its version, accepts only this repository's release
//! URLs, verifies the signature, downloads its asset, verifies the checksum, unpacks it beside
//! the installed copy, checks the new program reports the expected version, swaps it in
//! (macOS: the whole signed `zenith.app`, the previous one kept as `.zenith-previous.app`
//! until the new one starts) and restarts the server. `ZENITH_NO_UPDATE=1` turns all of it
//! off; `ZENITH_PRETEND_VERSION=0.0.1` exercises it against a real release.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const REPO: &str = "ludovic111/zenith";
const CHECKSUMS: &str = "SHA256SUMS";
const SIGNATURE: &str = "SHA256SUMS.sig";
const SIGNATURE_PREFIX: &str = "zenith-ed25519";
const MAX_ASSET: u64 = 1024 * 1024 * 1024;
/// Hex Ed25519 public key; empty while no release key exists (then nothing is installed).
const PUBLIC_KEY_HEX: &str = include_str!("../assets/update-signing.pub");

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub tag: String,
    pub notes: String,
    pub asset: String,
    pub url: String,
    pub sha256: Option<String>,
}

/// `ZENITH_NO_UPDATE=1` turns update checks and installs off.
pub fn disabled() -> bool {
    std::env::var("ZENITH_NO_UPDATE").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub fn current_version() -> String {
    std::env::var("ZENITH_PRETEND_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// The release asset for this platform.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("zenith-macos-arm64.zip")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("zenith-macos-x86_64.zip")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("zenith-linux-x86_64.tar.gz")
    } else {
        None
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    let hex: String = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn public_key() -> Option<[u8; 32]> {
    parse_hex32(PUBLIC_KEY_HEX)
}

pub fn verify_signature(message: &[u8], signature_text: &str) -> Result<()> {
    let key = public_key().ok_or("This build has no release signing key")?;
    let line = signature_text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(SIGNATURE_PREFIX))
        .ok_or("The release signature has an unknown format")?;
    let bytes = STANDARD
        .decode(line[SIGNATURE_PREFIX.len()..].trim())
        .map_err(|e| format!("The release signature is not valid base64: {e}"))?;
    let signature = Signature::from_slice(&bytes).map_err(|_| "The release signature has the wrong length")?;
    let verifying = VerifyingKey::from_bytes(&key).map_err(|_| "The built-in release key is invalid")?;
    verifying
        .verify(message, &signature)
        .map_err(|_| "The release signature does not match SHA256SUMS; nothing was changed".into())
}

/// Creates a signing key pair: the secret in `path` (0600, never committed), the public key
/// (hex) returned for `assets/update-signing.pub`.
pub fn write_keypair(path: &Path) -> Result<String> {
    if path.exists() {
        return Err(format!("{} exists; refusing to overwrite a signing key", path.display()));
    }
    let signing = SigningKey::generate(&mut rand_core::OsRng);
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    writeln!(file, "# zenith release signing secret key. Keep private.\n{}", hex(&signing.to_bytes())).map_err(|e| e.to_string())?;
    Ok(hex(signing.verifying_key().as_bytes()))
}

/// Signs `file` with the secret key (hex in a file, or `ZENITH_UPDATE_SIGNING_KEY`), writing
/// `<file>.sig`.
pub fn sign_file(key: Option<&Path>, file: &Path) -> Result<PathBuf> {
    let secret = match key {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
        None => std::env::var("ZENITH_UPDATE_SIGNING_KEY").map_err(|_| "no key file and no ZENITH_UPDATE_SIGNING_KEY")?,
    };
    let secret = parse_hex32(&secret).ok_or("The secret key must be 64 hex characters")?;
    let signing = SigningKey::from_bytes(&secret);
    let message = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let text = format!("{SIGNATURE_PREFIX} {}\n", STANDARD.encode(signing.sign(&message).to_bytes()));
    let out = PathBuf::from(format!("{}.sig", file.display()));
    std::fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(out)
}

fn trusted_url(url: &str) -> Result<()> {
    if url.starts_with(&format!("https://github.com/{REPO}/releases/download/")) {
        Ok(())
    } else {
        Err(format!("Refusing to download from an unexpected location: {url}"))
    }
}

pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?))
}

pub fn newer(latest: &str, current: &str) -> bool {
    let pre = |v: &str| v.trim().split('+').next().unwrap_or("").contains('-');
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c || (l == c && pre(current) && !pre(latest)),
        _ => false,
    }
}

pub fn parse_checksum(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

/// A release with the URLs of its `SHA256SUMS` and `SHA256SUMS.sig`.
pub type Found = (Release, Option<String>, Option<String>);

/// The release this platform can install, from a GitHub `releases/latest` document.
pub fn find(json: &Value, asset: &str) -> Result<Option<Found>> {
    let tag = json.get("tag_name").and_then(Value::as_str).ok_or("The release has no tag")?;
    let assets = json.get("assets").and_then(Value::as_array).ok_or("The release lists no assets")?;
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|a| a.get("browser_download_url")?.as_str().map(String::from))
    };
    let Some(url) = url_of(asset) else { return Ok(None) };
    trusted_url(&url)?;
    let sums = url_of(CHECKSUMS);
    let sig = url_of(SIGNATURE);
    for extra in sums.iter().chain(sig.iter()) {
        trusted_url(extra)?;
    }
    Ok(Some((
        Release {
            version: tag.trim_start_matches(['v', 'V']).to_owned(),
            tag: tag.to_owned(),
            notes: json.get("body").and_then(Value::as_str).unwrap_or_default().to_owned(),
            asset: asset.to_owned(),
            url,
            sha256: None,
        },
        sums,
        sig,
    )))
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("zenith/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    client
        .get(url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Could not reach GitHub: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Could not read the reply from GitHub: {e}"))
}

/// The latest release when it is newer than this build, signature and checksum checked.
pub async fn check() -> Result<Option<Release>> {
    if disabled() {
        return Ok(None);
    }
    let Some(asset) = asset_name() else { return Ok(None) };
    let client = http()?;
    let response = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let json: Value = response
        .error_for_status()
        .map_err(|e| format!("GitHub: {e}"))?
        .json()
        .await
        .map_err(|e| format!("GitHub sent an unexpected reply: {e}"))?;
    let Some((mut release, sums_url, sig_url)) = find(&json, asset)? else {
        return Ok(None);
    };
    if !newer(&release.version, &current_version()) {
        return Ok(None);
    }
    let sums_url = sums_url.ok_or_else(|| format!("Release {} has no {CHECKSUMS}", release.tag))?;
    let sig_url = sig_url.ok_or_else(|| format!("Release {} is not signed; refusing it", release.tag))?;
    let sums = get_text(&client, &sums_url).await?;
    let signature = get_text(&client, &sig_url).await?;
    verify_signature(sums.as_bytes(), &signature)?;
    release.sha256 = Some(parse_checksum(&sums, asset).ok_or_else(|| format!("{CHECKSUMS} of {} lacks {asset}", release.tag))?);
    Ok(Some(release))
}

async fn download(release: &Release, to: &Path) -> Result<()> {
    use futures::StreamExt;
    trusted_url(&release.url)?;
    let response = http()?
        .get(&release.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Download failed: {e}"))?;
    let mut file = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("Download failed: {e}"))?;
        total += chunk.len() as u64;
        if total > MAX_ASSET {
            return Err("The download is larger than any zenith release".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    match &release.sha256 {
        Some(expected) if *expected == hex(&hasher.finalize()) => Ok(()),
        _ => Err(format!(
            "The download of {} does not match its published checksum; nothing was changed",
            release.asset
        )),
    }
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let output = command.output().map_err(|e| format!("{what}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{what}: {}", String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn reports_version(binary: &Path, version: &str) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .env_remove("ZENITH_PRETEND_VERSION")
        .output()
        .map_err(|e| format!("Could not run the new zenith: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if text.split_whitespace().any(|word| word.trim_start_matches('v') == version) {
        Ok(())
    } else {
        Err(format!("The downloaded zenith reports {:?}, not {version}; nothing was changed", text.trim()))
    }
}

/// Downloads, verifies and installs `release`; returns what to launch afterwards.
pub async fn install(release: &Release) -> Result<PathBuf> {
    if disabled() {
        return Err("Updates are off (ZENITH_NO_UPDATE)".into());
    }
    let dir = zenith_client::local::app_home().join("updates").join(&release.version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join(&release.asset);
    download(release, &archive).await?;
    let installed = install_archive(&archive, &dir, &release.version)?;
    // The server runs from the installed copy: restart it on the new one.
    zenith_client::local::restart_server();
    Ok(installed)
}

#[cfg(target_os = "macos")]
fn install_archive(archive: &Path, dir: &Path, version: &str) -> Result<PathBuf> {
    let bundle = crate::lsuite::bundle_path().ok_or("Updates apply to an installed zenith.app; this copy runs from a build folder")?;
    let staging = dir.join("staged");
    run(
        Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(archive).arg(&staging),
        "Unpacking the update",
    )?;
    let new_app = staging.join("zenith.app");
    let new_binary = new_app.join("Contents/MacOS/zenith");
    if !new_binary.is_file() {
        return Err("The update does not contain zenith.app".into());
    }
    run(
        Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&new_app),
        "Checking the update's signature",
    )?;
    reports_version(&new_binary, version)?;
    let previous = bundle.with_file_name(".zenith-previous.app");
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::rename(&bundle, &previous).map_err(|e| format!("Could not move the current zenith aside: {e}"))?;
    if let Err(error) = std::fs::rename(&new_app, &bundle) {
        let _ = std::fs::rename(&previous, &bundle);
        return Err(format!("Could not put the new zenith in place: {error}"));
    }
    Ok(bundle)
}

#[cfg(not(target_os = "macos"))]
fn install_archive(archive: &Path, dir: &Path, version: &str) -> Result<PathBuf> {
    let staging = dir.join("staged");
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    run(Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&staging), "Unpacking the update")?;
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    let install_dir = here.parent().ok_or("no install folder")?.to_path_buf();
    let names = ["zenith", "zenith-code", "zenith-cli", "zenith-mcp"];
    for name in names {
        if !staging.join(name).is_file() {
            return Err(format!("The update lacks {name}"));
        }
    }
    reports_version(&staging.join("zenith"), version)?;
    for name in names {
        let target = install_dir.join(name);
        let backup = install_dir.join(format!(".{name}.previous"));
        let _ = std::fs::remove_file(&backup);
        if target.exists() {
            std::fs::rename(&target, &backup).map_err(|e| format!("{name}: {e}"))?;
        }
        std::fs::rename(staging.join(name), &target).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(install_dir.join("zenith"))
}

/// After a successful start on a new version: the previous copy can go.
pub fn forget_previous() {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = crate::lsuite::bundle_path() {
        let _ = std::fs::remove_dir_all(bundle.with_file_name(".zenith-previous.app"));
    }
    let _ = std::fs::remove_dir_all(zenith_client::local::app_home().join("updates"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions() {
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(newer("0.2.0", "0.2.0-rc.1"));
        assert!(!newer("garbage", "0.1.0"));
    }

    #[test]
    fn only_this_repository() {
        let release = json!({
            "tag_name": "v9.0.0",
            "assets": [
                {"name": "zenith-macos-arm64.zip", "browser_download_url": "https://github.com/ludovic111/zenith/releases/download/v9.0.0/zenith-macos-arm64.zip"},
                {"name": "SHA256SUMS", "browser_download_url": "https://example.com/SHA256SUMS"}
            ]
        });
        assert!(find(&release, "zenith-macos-arm64.zip").is_err());
        assert_eq!(
            parse_checksum(&format!("{}  zenith-macos-arm64.zip\n", "a".repeat(64)), "zenith-macos-arm64.zip"),
            Some("a".repeat(64))
        );
    }

    #[test]
    fn signatures_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        let public = write_keypair(&key).unwrap();
        assert_eq!(public.len(), 64);
        let file = dir.path().join("SHA256SUMS");
        std::fs::write(&file, "made-up sums\n").unwrap();
        let sig = sign_file(Some(&key), &file).unwrap();
        let text = std::fs::read_to_string(sig).unwrap();
        assert!(text.starts_with("zenith-ed25519 "));
        // Verifying needs the compiled-in key; check the format at least.
        let bytes = STANDARD.decode(text["zenith-ed25519 ".len()..].trim()).unwrap();
        assert_eq!(bytes.len(), 64);
    }
}
