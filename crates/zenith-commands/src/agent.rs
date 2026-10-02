//! The server's LaunchAgent (macOS): zenith's server runs on its own from login, so the window
//! can close and the agents keep working. An installed `zenith.app` carries the server
//! (`Contents/MacOS/zenith-code`) and the web interface (`Contents/Resources/web`); when the app
//! starts, [`ensure`] writes `~/Library/LaunchAgents/<label>.plist` to run that server, and
//! reloads it when the plist pointed elsewhere (an older copy, a moved app). A plist that runs
//! something else (`ZENITH_SERVER=node` from `scripts/mac/install.sh`) is left alone.

use std::path::{Path, PathBuf};

/// What the agent should run, from the app bundle this program is in.
pub fn bundled_server() -> Option<(PathBuf, Option<PathBuf>)> {
    let bundle = crate::lsuite::bundle_path()?;
    let server = bundle.join("Contents/MacOS/zenith-code");
    if !server.is_file() {
        return None;
    }
    let web = bundle.join("Contents/Resources/web");
    Some((server, web.join("index.html").is_file().then_some(web)))
}

fn plist_path(label: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The port of `http://host:port`.
fn port(base_url: &str) -> u16 {
    base_url.rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse().ok()).unwrap_or(4747)
}

/// The plist for `program` (the server, its arguments) with logs in `~/Library/Logs/Zenith`.
pub fn plist(label: &str, arguments: &[String], home: &Path) -> String {
    let logs = home.join("Library/Logs/Zenith/server.log");
    let args: String = arguments.iter().map(|a| format!("    <string>{}</string>\n", xml(a))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>ZENITH_NO_STARTUP_TOKEN</key><string>1</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{logs}</string>
  <key>StandardErrorPath</key><string>{logs}</string>
</dict>
</plist>
"#,
        label = xml(label),
        logs = xml(&logs.to_string_lossy()),
    )
}

/// The first `<string>` of `ProgramArguments` in a plist.
fn program_of(plist: &str) -> Option<String> {
    let after = plist.split("<key>ProgramArguments</key>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(
        after[start..end]
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\""),
    )
}

/// Installs or repoints the LaunchAgent when this program runs from a zenith.app that carries
/// the server, unless the plist runs something else. Returns whether it changed anything.
pub fn ensure() -> anyhow::Result<bool> {
    install(false)
}

/// Like [`ensure`]; `force` also replaces a plist that runs something else
/// (`zenith-cli setup`, run by `scripts/mac/install.sh`).
#[cfg(target_os = "macos")]
pub fn install(force: bool) -> anyhow::Result<bool> {
    let Some((server, web)) = bundled_server() else { return Ok(false) };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let label = zenith_client::local::agent_label();
    let path = plist_path(&label);
    let mut arguments = vec![
        server.to_string_lossy().into_owned(),
        "serve".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port(&zenith_client::local::base_url()).to_string(),
        "--base-dir".into(),
        zenith_client::local::code_home().to_string_lossy().into_owned(),
    ];
    if let Some(web) = web {
        arguments.push("--static-dir".into());
        arguments.push(web.to_string_lossy().into_owned());
    }
    let wanted = plist(&label, &arguments, &home);
    if let Ok(current) = std::fs::read_to_string(&path) {
        if current == wanted {
            return Ok(false);
        }
        // Someone else's server (the TypeScript one, a build folder): not ours to change.
        let ours = program_of(&current).is_some_and(|p| p.ends_with("/Contents/MacOS/zenith-code"));
        if !ours && !force {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(home.join("Library/Logs/Zenith"))?;
    std::fs::create_dir_all(zenith_client::local::code_home())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, wanted)?;
    // SAFETY: getuid has no preconditions.
    let domain = format!("gui/{}", unsafe { getuid() });
    let launchctl = |args: &[&str]| std::process::Command::new("/bin/launchctl").args(args).output();
    let _ = launchctl(&["bootout", &format!("{domain}/{label}")]);
    for _ in 0..20 {
        let listed = launchctl(&["print", &format!("{domain}/{label}")]).map(|o| o.status.success()).unwrap_or(false);
        if !listed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let path_text = path.to_string_lossy().into_owned();
    let output = launchctl(&["bootstrap", &domain, &path_text])?;
    if !output.status.success() {
        anyhow::bail!("launchctl bootstrap: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let _ = launchctl(&["kickstart", "-k", &format!("{domain}/{label}")]);
    Ok(true)
}

#[cfg(target_os = "macos")]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(not(target_os = "macos"))]
pub fn install(_force: bool) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_name_their_program() {
        let text = plist(
            "dev.zenith.app",
            &[
                "/Applications/zenith.app/Contents/MacOS/zenith-code".into(),
                "serve".into(),
                "--base-dir".into(),
                "/Users/me/a & b".into(),
            ],
            Path::new("/Users/me"),
        );
        assert!(text.contains("<string>/Users/me/a &amp; b</string>"));
        assert_eq!(program_of(&text).as_deref(), Some("/Applications/zenith.app/Contents/MacOS/zenith-code"));
        assert_eq!(port("http://127.0.0.1:4799"), 4799);
        assert_eq!(port("http://127.0.0.1:4747/"), 4747);
    }
}
