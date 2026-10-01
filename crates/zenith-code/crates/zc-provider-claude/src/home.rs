//! `provider/Drivers/ClaudeHome.ts` and `ClaudeExecutable.ts`: where an instance's Claude
//! config lives (`CLAUDE_CONFIG_DIR`, never `HOME`, which would hide the macOS keychain), the
//! continuation and cache keys derived from it, the signed-out message, and the executable the
//! driver spawns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zc_core::paths::{expand_home_path, home_dir, normalize_lexically, resolve_path};

/// A child-process environment.
pub type Env = BTreeMap<String, String>;

/// The current process environment, as an [`Env`].
pub fn process_env() -> Env {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// `resolveClaudeHomePath({homePath}, environment)`: the instance's `homePath`, else an
/// inherited `CLAUDE_CONFIG_DIR`, else `~/.claude`, made absolute.
pub fn resolve_claude_home_path(home_path: &str, environment: Option<&Env>) -> PathBuf {
    let home_path = home_path.trim();
    if !home_path.is_empty() {
        return resolve_path(&expand_home_path(home_path));
    }
    let inherited = environment.and_then(|env| env.get("CLAUDE_CONFIG_DIR")).map(|v| v.trim()).unwrap_or_default();
    if !inherited.is_empty() {
        return resolve_path(Path::new(inherited));
    }
    resolve_path(&home_dir().join(".claude"))
}

/// `makeClaudeEnvironment({homePath}, baseEnv)`: the base environment, plus
/// `CLAUDE_CONFIG_DIR` when the instance has its own home.
pub fn make_claude_environment(home_path: &str, base_env: Env) -> Env {
    if home_path.trim().is_empty() {
        return base_env;
    }
    let mut env = base_env;
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        resolve_claude_home_path(home_path, None).to_string_lossy().into_owned(),
    );
    env
}

/// `makeClaudeContinuationGroupKey`: `claude:home:<resolved config dir>`.
pub fn continuation_group_key(home_path: &str, environment: Option<&Env>) -> String {
    format!("claude:home:{}", resolve_claude_home_path(home_path, environment).display())
}

/// `makeClaudeCapabilitiesCacheKey`: `<binary>\0<config dir>\0<cwd>`.
pub fn capabilities_cache_key(binary_path: &str, home_path: &str, cwd: Option<&str>, environment: Option<&Env>) -> String {
    format!(
        "{binary_path}\0{}\0{}",
        resolve_claude_home_path(home_path, environment).display(),
        cwd.unwrap_or_default()
    )
}

/// `claudeSignedOutMessage({configDir, cwd})`.
pub fn claude_signed_out_message(config_dir: Option<&str>, cwd: &str) -> String {
    let quote = |value: &str| serde_json::to_string(value).unwrap_or_default();
    let configuration = config_dir
        .map(|dir| format!(" from {}, with CLAUDE_CONFIG_DIR set to {}", quote(cwd), quote(dir)))
        .unwrap_or_default();
    format!(
        "Claude could not authenticate. For subscription login, run `claude auth login` on this environment's machine{configuration}, then start a new thread. For API-key authentication, check this instance's configured credentials."
    )
}

/// `resolveClaudeSdkExecutablePath(binaryPath, environment)`: what to spawn. Off Windows the
/// configured value is used as is; on Windows a bare command and an npm launcher shim are
/// followed to the package entry (`bin/claude.exe`, or `cli.js` for old packages).
pub fn resolve_claude_executable_path(binary_path: &str, environment: &Env) -> String {
    if !cfg!(windows) {
        return binary_path.to_string();
    }
    resolve_windows_executable(binary_path, environment, |path| path.is_file())
}

/// The Windows branch of [`resolve_claude_executable_path`] with an injectable file check.
pub fn resolve_windows_executable(binary_path: &str, environment: &Env, is_file: impl Fn(&Path) -> bool) -> String {
    const SHIM_EXTENSIONS: [&str; 3] = [".cmd", ".bat", ".ps1"];
    let resolved = resolve_on_windows_path(binary_path, environment, &is_file).unwrap_or_else(|| binary_path.to_string());
    let lower = resolved.to_lowercase();
    if !SHIM_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        return resolved;
    }
    let shim_dir = match resolved.rfind(['\\', '/']) {
        Some(index) => &resolved[..index],
        None => ".",
    };
    for segments in [
        ["node_modules", "@anthropic-ai", "claude-code", "bin", "claude.exe"].as_slice(),
        ["node_modules", "@anthropic-ai", "claude-code", "cli.js"].as_slice(),
    ] {
        let candidate = format!("{shim_dir}\\{}", segments.join("\\"));
        if is_file(Path::new(&candidate)) {
            return candidate;
        }
    }
    tracing::warn!(
        binary_path,
        resolved,
        "Claude launcher shim resolved but no known package entry was found next to it"
    );
    binary_path.to_string()
}

fn resolve_on_windows_path(command: &str, environment: &Env, is_file: &impl Fn(&Path) -> bool) -> Option<String> {
    let has_separator = command.contains(['\\', '/']);
    let extensions: Vec<String> = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATHEXT"))
        .map(|(_, value)| value.split(';').filter(|e| !e.is_empty()).map(str::to_lowercase).collect())
        .unwrap_or_else(|| vec![".com".into(), ".exe".into(), ".bat".into(), ".cmd".into()]);
    let with_extensions = |base: &str| -> Option<String> {
        let lower = base.to_lowercase();
        if extensions.iter().any(|ext| lower.ends_with(ext.as_str())) && is_file(Path::new(base)) {
            return Some(base.to_string());
        }
        extensions
            .iter()
            .map(|ext| format!("{base}{ext}"))
            .find(|candidate| is_file(Path::new(candidate)))
    };
    if has_separator {
        return with_extensions(command);
    }
    let path = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())?;
    path.split(';')
        .filter(|dir| !dir.is_empty())
        .find_map(|dir| with_extensions(&format!("{}\\{command}", dir.trim_end_matches(['\\', '/']))))
}

/// `path.resolve(cwd ?? ".")`.
pub fn resolve_cwd(cwd: Option<&str>) -> PathBuf {
    resolve_path(Path::new(cwd.unwrap_or(".")))
}

/// `path.resolve(base, value)` (two arguments).
pub fn resolve_against(base: &Path, value: &str) -> PathBuf {
    let value = Path::new(value);
    if value.is_absolute() {
        normalize_lexically(value)
    } else {
        normalize_lexically(&resolve_path(base).join(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_home_path_precedence() {
        let env = Env::from([("CLAUDE_CONFIG_DIR".to_string(), "/tmp/inherited-claude".to_string())]);
        assert_eq!(resolve_claude_home_path(" /tmp/explicit ", Some(&env)), PathBuf::from("/tmp/explicit"));
        assert_eq!(resolve_claude_home_path("", Some(&env)), PathBuf::from("/tmp/inherited-claude"));
        assert_eq!(resolve_claude_home_path("", None), home_dir().join(".claude"));
        assert_eq!(resolve_claude_home_path("~/.claude-work", None), home_dir().join(".claude-work"));
    }

    #[test]
    fn sets_claude_config_dir_only_for_an_explicit_home() {
        let base = Env::from([("PATH".to_string(), "/bin".to_string())]);
        assert_eq!(make_claude_environment("", base.clone()), base);
        let env = make_claude_environment("/tmp/claude-home", base);
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/tmp/claude-home"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    }

    #[test]
    fn keys_continuation_and_caches_by_the_resolved_home() {
        assert_eq!(continuation_group_key("/tmp/a", None), "claude:home:/tmp/a");
        assert_eq!(capabilities_cache_key("claude", "/tmp/a", Some("/w"), None), "claude\0/tmp/a\0/w");
    }

    #[test]
    fn quotes_paths_in_the_signed_out_message() {
        let message = claude_signed_out_message(Some("/tmp/my claude"), "/work");
        assert!(message.contains(r#"from "/work", with CLAUDE_CONFIG_DIR set to "/tmp/my claude""#));
        assert!(!claude_signed_out_message(None, "/work").contains("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn follows_windows_npm_shims_to_the_package_entry() {
        let env = Env::from([("PATH".to_string(), r"C:\npm".to_string()), ("PATHEXT".to_string(), ".EXE;.CMD".to_string())]);
        let files = [r"C:\npm\claude.cmd", r"C:\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe"];
        let is_file = |path: &Path| files.iter().any(|f| Path::new(f) == path);
        assert_eq!(
            resolve_windows_executable("claude", &env, is_file),
            r"C:\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe"
        );
        let files_old = [r"C:\npm\claude.cmd", r"C:\npm\node_modules\@anthropic-ai\claude-code\cli.js"];
        let is_file_old = |path: &Path| files_old.iter().any(|f| Path::new(f) == path);
        assert_eq!(
            resolve_windows_executable("claude", &env, is_file_old),
            r"C:\npm\node_modules\@anthropic-ai\claude-code\cli.js"
        );
        assert_eq!(resolve_windows_executable("claude", &env, |_| false), "claude");
    }

    // Ports of `ClaudeHome.test.ts`.

    #[test]
    fn treats_empty_tilde_claude_and_the_expanded_default_as_the_same_claude_home() {
        let resolved = home_dir().join(".claude");
        let resolved_text = resolved.to_string_lossy().into_owned();
        assert_eq!(resolve_claude_home_path("", None), resolved);
        assert_eq!(resolve_claude_home_path("~/.claude", None), resolved);
        assert_eq!(resolve_claude_home_path(&resolved_text, None), resolved);
        let base = Env::from([("PATH".to_string(), "/bin".to_string())]);
        assert_eq!(make_claude_environment("", base.clone()), base);

        let key = format!("claude:home:{resolved_text}");
        assert_eq!(continuation_group_key("", None), key);
        assert_eq!(continuation_group_key("~/.claude", None), key);
        assert_eq!(continuation_group_key(&resolved_text, None), key);
    }

    #[test]
    fn resolves_configured_claude_home_and_stamps_continuation_and_cache_keys_with_it() {
        let home_path = "~/.claude-work";
        let resolved = home_dir().join(".claude-work");
        let resolved_text = resolved.to_string_lossy().into_owned();
        assert_eq!(resolve_claude_home_path(home_path, None), resolved);
        assert_eq!(make_claude_environment(home_path, Env::new()).get("CLAUDE_CONFIG_DIR"), Some(&resolved_text));
        assert_eq!(continuation_group_key(home_path, None), format!("claude:home:{resolved_text}"));
        assert_eq!(capabilities_cache_key("claude", home_path, None, None), format!("claude\0{resolved_text}\0"));
    }

    #[test]
    fn uses_inherited_claude_config_dir_when_home_path_is_empty() {
        let inherited = resolve_path(Path::new("/tmp/claude-inherited"));
        let environment = Env::from([("CLAUDE_CONFIG_DIR".to_string(), inherited.to_string_lossy().into_owned())]);
        assert_eq!(resolve_claude_home_path("", Some(&environment)), inherited);
        assert_eq!(continuation_group_key("", Some(&environment)), format!("claude:home:{}", inherited.display()));
        assert_eq!(resolve_claude_home_path("~/.claude-work", Some(&environment)), home_dir().join(".claude-work"));
    }

    #[test]
    fn points_the_signed_out_hint_at_the_configured_claude_home() {
        assert!(claude_signed_out_message(None, "/synthetic").contains("run `claude auth login`"));
        let config_dir = "/synthetic/Claude work's $literal";
        let message = claude_signed_out_message(Some(config_dir), "/synthetic/project");
        assert!(message.contains(&format!("CLAUDE_CONFIG_DIR set to \"{config_dir}\"")), "{message}");
        assert!(!message.contains("CLAUDE_CONFIG_DIR="));
        assert!(message.contains("then start a new thread"));
    }

    #[test]
    fn separates_capability_probes_by_cwd() {
        let first = capabilities_cache_key("claude", "", Some("/repo-a"), None);
        let second = capabilities_cache_key("claude", "", Some("/repo-b"), None);
        assert_ne!(first, second);
    }

    // Ports of `ClaudeExecutable.test.ts`. TS injects the command resolution; here the Windows
    // branch resolves against `PATH`/`PATHEXT` with an injected file check.

    const NPM_DIR: &str = r"C:\Users\dev\AppData\Roaming\npm";

    fn npm_package_exe() -> String {
        format!(r"{NPM_DIR}\node_modules\@anthropic-ai\claude-code\bin\claude.exe")
    }

    fn npm_package_cli() -> String {
        format!(r"{NPM_DIR}\node_modules\@anthropic-ai\claude-code\cli.js")
    }

    fn windows_env(path_dir: &str) -> Env {
        Env::from([
            ("PATH".to_string(), path_dir.to_string()),
            ("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD;.PS1".to_string()),
        ])
    }

    fn existing(files: Vec<String>) -> impl Fn(&Path) -> bool {
        move |path: &Path| files.iter().any(|f| Path::new(f) == path)
    }

    #[cfg(not(windows))]
    #[test]
    fn returns_the_configured_path_unchanged_on_non_windows_platforms() {
        assert_eq!(resolve_claude_executable_path("claude", &Env::new()), "claude");
    }

    #[test]
    fn returns_the_resolved_absolute_path_for_native_windows_executables() {
        let native_binary = r"C:\Users\dev\.local\bin\claude.exe";
        assert_eq!(
            resolve_windows_executable("claude", &windows_env(r"C:\Users\dev\.local\bin"), existing(vec![native_binary.to_string()])),
            native_binary
        );
    }

    #[test]
    fn follows_an_npm_launcher_shim_to_the_packaged_native_binary() {
        let files = vec![format!(r"{NPM_DIR}\claude.cmd"), npm_package_exe(), npm_package_cli()];
        assert_eq!(resolve_windows_executable("claude", &windows_env(NPM_DIR), existing(files)), npm_package_exe());
    }

    #[test]
    fn follows_bat_and_ps1_launcher_shims_the_same_way() {
        for shim in [format!(r"{NPM_DIR}\claude.bat"), format!(r"{NPM_DIR}\claude.ps1")] {
            assert_eq!(
                resolve_windows_executable("claude", &windows_env(NPM_DIR), existing(vec![shim.clone(), npm_package_exe()])),
                npm_package_exe(),
                "{shim}"
            );
        }
    }

    #[test]
    fn normalizes_mixed_case_shim_extensions_before_matching() {
        let shim = format!(r"{NPM_DIR}\claude.CMD");
        assert_eq!(
            resolve_windows_executable(&shim, &windows_env(NPM_DIR), existing(vec![shim.clone(), npm_package_exe()])),
            npm_package_exe()
        );
    }

    #[test]
    fn falls_back_to_cli_js_when_the_package_ships_no_native_binary() {
        let files = vec![format!(r"{NPM_DIR}\claude.cmd"), npm_package_cli()];
        assert_eq!(resolve_windows_executable("claude", &windows_env(NPM_DIR), existing(files)), npm_package_cli());
    }

    #[test]
    fn returns_the_configured_path_when_a_shim_has_no_known_package_entry() {
        assert_eq!(
            resolve_windows_executable("claude", &windows_env(NPM_DIR), existing(vec![format!(r"{NPM_DIR}\claude.cmd")])),
            "claude"
        );
    }

    #[test]
    fn returns_the_configured_path_when_command_resolution_finds_nothing() {
        assert_eq!(resolve_windows_executable("claude", &windows_env(NPM_DIR), existing(Vec::new())), "claude");
    }
}
