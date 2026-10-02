//! The server's LaunchAgent (macOS) and systemd user service (Linux): zenith's server runs on
//! its own, so the window can close and the agents keep working.
//!
//! macOS: an installed `zenith.app` carries the server
//! (`Contents/MacOS/zenith-code`) and the web interface (`Contents/Resources/web`); when the app
//! starts, [`ensure`] writes `~/Library/LaunchAgents/<label>.plist` to run that server, and
//! reloads it when the plist pointed elsewhere (an older copy, a moved app). A plist that runs
//! something else (`ZENITH_SERVER=node` from `scripts/mac/install.sh`) is left alone.
//!
//! Linux: the release archive holds the server and the web interface (`client`, which the
//! server finds next to itself) beside `zenith-cli`. `zenith-cli setup` ([`setup`]) writes
//! `~/.config/systemd/user/zenith.service` to run that server, enables it, and turns lingering
//! on so it starts when the machine boots, before anyone logs in. It always listens on
//! `127.0.0.1`; `--tailscale-serve` also publishes it on the tailnet through Tailscale Serve.

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

#[cfg(target_os = "macos")]
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
#[cfg(any(target_os = "macos", test))]
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

/// What `zenith-cli setup` was asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Setup {
    /// Publish the server on the tailnet with Tailscale Serve (HTTPS).
    pub tailscale_serve: bool,
    /// The tailnet port (the server's default is 443).
    pub tailscale_serve_port: Option<u16>,
}

impl Setup {
    /// `zenith-cli setup`'s arguments.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut setup = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--tailscale-serve" => setup.tailscale_serve = true,
                "--tailscale-serve-port" => {
                    let port = args.next().and_then(|p| p.parse().ok()).filter(|p| *p > 0);
                    setup.tailscale_serve_port = Some(port.ok_or("--tailscale-serve-port needs a port (1-65535)")?);
                    setup.tailscale_serve = true;
                }
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        Ok(setup)
    }
}

/// One word of a systemd command line: quoted when it has to be, with `%` and `$` doubled
/// (systemd expands both).
fn unit_word(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    if escaped.is_empty() || escaped.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';')) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

/// The systemd user unit that runs `server` on `127.0.0.1:port` with its state in `base_dir`
/// and its log in `log`. The address is not an option: without `--host` the server listens on
/// every interface. Other machines reach it through Tailscale Serve, which forwards the
/// tailnet's HTTPS to the loopback port.
pub fn unit(server: &Path, port: u16, base_dir: &Path, log: &Path, setup: &Setup) -> String {
    let mut command = vec![
        unit_word(&server.to_string_lossy()),
        "serve --host 127.0.0.1 --port".into(),
        port.to_string(),
        "--base-dir".into(),
        unit_word(&base_dir.to_string_lossy()),
    ];
    if setup.tailscale_serve {
        command.push("--tailscale-serve".into());
        if let Some(port) = setup.tailscale_serve_port {
            command.push(format!("--tailscale-serve-port {port}"));
        }
    }
    format!(
        "# Written by `zenith-cli setup`; run it again rather than editing this file.
[Unit]
Description=zenith server (zenith-code)

[Service]
ExecStart={command}
Environment=ZENITH_NO_STARTUP_TOKEN=1
Restart=always
RestartSec=5
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
",
        command = command.join(" "),
        log = log.to_string_lossy().replace('%', "%%"),
    )
}

/// `zenith-cli setup`: makes the server next to this program start on its own (at login on
/// macOS, at boot on Linux) and says what it did.
#[cfg(target_os = "macos")]
pub fn setup(setup: &Setup) -> anyhow::Result<String> {
    if setup.tailscale_serve {
        anyhow::bail!("--tailscale-serve is for Linux; on macOS the server answers on 127.0.0.1 only");
    }
    Ok(if install(true)? {
        format!(
            "The server now starts at login from {}.",
            crate::lsuite::bundle_path().map(|p| p.display().to_string()).unwrap_or_default()
        )
    } else {
        "Nothing to do (already set up, or not run from an installed zenith.app).".into()
    })
}

#[cfg(target_os = "linux")]
pub fn setup(setup: &Setup) -> anyhow::Result<String> {
    use zenith_client::local;
    let server = crate::lsuite::sibling("zenith-code")
        .ok_or_else(|| anyhow::anyhow!("zenith-code is not next to zenith-cli: extract the whole release archive in one folder"))?;
    let base_url = local::base_url();
    let log = local::server_log();
    let unit_name = local::service_unit();
    let path = local::service_unit_path();
    let wanted = unit(&server, port(&base_url), &local::code_home(), &log, setup);
    let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(wanted.as_str());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    // A server started some other way holds the port: the service could never start.
    let running = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &unit_name])
        .status();
    if !running.is_ok_and(|status| status.success()) && runtime.block_on(local::healthy(&base_url)) {
        anyhow::bail!("a zenith server that {unit_name} did not start already answers on {base_url}: stop it, then run `zenith-cli setup` again");
    }
    std::fs::create_dir_all(local::code_home())?;
    for dir in [log.parent(), path.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir)?;
    }
    if changed {
        std::fs::write(&path, wanted)?;
    }
    let systemctl = |args: &[&str]| -> anyhow::Result<bool> {
        let output = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("cannot run systemctl: {e} (zenith-cli setup needs systemd)"))?;
        Ok(output.status.success())
    };
    let must = |args: &[&str]| -> anyhow::Result<()> {
        if systemctl(args)? {
            Ok(())
        } else {
            anyhow::bail!("systemctl --user {} failed (see `systemctl --user status {unit_name}`)", args.join(" "))
        }
    };
    must(&["daemon-reload"])?;
    must(&["enable", &unit_name])?;
    // A new unit takes effect on a restart; an unchanged one is only started if it sleeps.
    must(&[if changed { "restart" } else { "start" }, &unit_name])?;
    let boot = lingering();
    let mut healthy = false;
    for _ in 0..40 {
        if runtime.block_on(local::healthy(&base_url)) && systemctl(&["is-active", "--quiet", &unit_name])? {
            healthy = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !healthy {
        anyhow::bail!(
            "{unit_name} is installed but the server does not answer on {base_url}: see {} and `systemctl --user status {unit_name}` (is something else on the port?)",
            log.display()
        );
    }
    let mut message = format!(
        "The server runs from {} on {base_url} ({unit_name}, {}); its log is {}.",
        server.display(),
        if changed { "written" } else { "unchanged" },
        log.display()
    );
    message.push_str(&match boot {
        Ok(()) => "\nIt starts when this machine boots.".to_owned(),
        Err(user) => format!("\nIt starts when you log in. To start it at boot: sudo loginctl enable-linger {user}"),
    });
    message.push_str(if setup.tailscale_serve {
        "\nTailscale Serve publishes it on your tailnet (`tailscale serve status` shows the address)."
    } else {
        "\nIt answers on this machine only (`zenith-cli setup --tailscale-serve` publishes it on your tailnet)."
    });
    Ok(message)
}

/// Makes the user's services start at boot rather than at login (`loginctl enable-linger`);
/// the user's name when that needs an administrator.
#[cfg(target_os = "linux")]
fn lingering() -> Result<(), String> {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| {
        let output = std::process::Command::new("id").arg("-un").output().ok()?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()).filter(|u| !u.is_empty())
    });
    let user = user.unwrap_or_default();
    if !user.is_empty() && Path::new("/var/lib/systemd/linger").join(&user).exists() {
        return Ok(());
    }
    let enabled = std::process::Command::new("loginctl")
        .arg("enable-linger")
        .args((!user.is_empty()).then_some(&user))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if enabled {
        Ok(())
    } else {
        Err(user)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn setup(_setup: &Setup) -> anyhow::Result<String> {
    Ok("Nothing to do (zenith-cli setup is for macOS and Linux).".into())
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

    #[test]
    fn units_listen_on_loopback_only() {
        let log = Path::new("/home/me/.local/state/zenith/server.log");
        let text = unit(
            Path::new("/opt/zenith/zenith-code"),
            4747,
            Path::new("/home/me/.zenith/code"),
            log,
            &Setup::default(),
        );
        assert!(text.contains("\nExecStart=/opt/zenith/zenith-code serve --host 127.0.0.1 --port 4747 --base-dir /home/me/.zenith/code\n"));
        assert!(text.contains("\nStandardOutput=append:/home/me/.local/state/zenith/server.log\n"));
        assert!(text.contains("\nRestart=always\n"));
        assert!(text.contains("\nWantedBy=default.target\n"));
        assert!(!text.contains("tailscale"));
    }

    #[test]
    fn units_can_serve_the_tailnet_and_still_listen_on_loopback() {
        let log = Path::new("/home/me/.local/state/zenith/server.log");
        for (setup, end) in [
            (
                Setup {
                    tailscale_serve: true,
                    tailscale_serve_port: None,
                },
                " --tailscale-serve\n",
            ),
            (
                Setup {
                    tailscale_serve: true,
                    tailscale_serve_port: Some(8443),
                },
                " --tailscale-serve --tailscale-serve-port 8443\n",
            ),
        ] {
            let text = unit(Path::new("/opt/zenith/zenith-code"), 4799, Path::new("/home/me/.zenith/code"), log, &setup);
            let command = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
            assert!(command.contains(" serve --host 127.0.0.1 --port 4799 "), "{command}");
            assert!(format!("{command}\n").ends_with(end), "{command}");
        }
    }

    #[test]
    fn unit_words_are_quoted_for_systemd() {
        assert_eq!(unit_word("/opt/zenith/zenith-code"), "/opt/zenith/zenith-code");
        assert_eq!(unit_word("/home/me/my code"), "\"/home/me/my code\"");
        assert_eq!(unit_word("/home/me/50%"), "/home/me/50%%");
        assert_eq!(unit_word("/home/me/$x"), "/home/me/$$x");
        assert_eq!(unit_word("a\"b"), "\"a\\\"b\"");
        let text = unit(
            Path::new("/opt/my zenith/zenith-code"),
            4747,
            Path::new("/home/me/50% done"),
            Path::new("/tmp/50%/server.log"),
            &Setup::default(),
        );
        assert!(text.contains("ExecStart=\"/opt/my zenith/zenith-code\" serve --host 127.0.0.1 --port 4747 --base-dir \"/home/me/50%% done\"\n"));
        assert!(text.contains("StandardError=append:/tmp/50%%/server.log\n"));
    }

    #[test]
    fn setup_arguments() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Setup::parse(&[]), Ok(Setup::default()));
        assert_eq!(
            Setup::parse(&args(&["--tailscale-serve"])),
            Ok(Setup {
                tailscale_serve: true,
                tailscale_serve_port: None
            })
        );
        assert_eq!(
            Setup::parse(&args(&["--tailscale-serve-port", "8443"])),
            Ok(Setup {
                tailscale_serve: true,
                tailscale_serve_port: Some(8443)
            })
        );
        assert!(Setup::parse(&args(&["--tailscale-serve-port", "0"])).is_err());
        assert!(Setup::parse(&args(&["--host", "0.0.0.0"])).is_err());
    }
}
