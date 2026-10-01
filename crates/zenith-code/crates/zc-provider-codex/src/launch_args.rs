//! Launch arguments (`provider/Layers/codexLaunchArgs.ts`, `@t3tools/shared/cliArgs`
//! `tokenizeCliArgs`).

use std::collections::BTreeMap;

/// The variable that overrides the configured launch args.
pub const T3CODE_CODEX_LAUNCH_ARGS_ENV: &str = "T3CODE_CODEX_LAUNCH_ARGS";

/// An environment as the TS server passes it (`NodeJS.ProcessEnv`).
pub type Environment = BTreeMap<String, String>;

/// `resolveCodexLaunchArgs`: `T3CODE_CODEX_LAUNCH_ARGS` (trimmed, when not blank) wins over the
/// configured args (trimmed).
pub fn resolve_codex_launch_args(launch_args: Option<&str>, environment: &Environment) -> String {
    if let Some(value) = environment.get(T3CODE_CODEX_LAUNCH_ARGS_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    launch_args.map(str::trim).unwrap_or_default().to_owned()
}

/// `tokenizeCliArgs`: shell-like splitting with single and double quotes; in double quotes a
/// backslash escapes `"`, `\`, `$` and `` ` ``; outside quotes it escapes whitespace only.
pub fn tokenize_cli_args(args: Option<&str>) -> Vec<String> {
    let Some(input) = args.map(str::trim).filter(|input| !input.is_empty()) else {
        return Vec::new();
    };
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        if let Some(open) = quote {
            if char == open {
                quote = None;
                quoted = true;
            } else if char == '\\' && open == '"' {
                match chars.get(index + 1) {
                    Some(&next) if matches!(next, '"' | '\\' | '$' | '`') => {
                        current.push(next);
                        index += 1;
                    }
                    _ => current.push(char),
                }
            } else {
                current.push(char);
            }
            index += 1;
            continue;
        }
        if char == '\'' || char == '"' {
            quote = Some(char);
            quoted = true;
        } else if is_js_whitespace(char) {
            if !current.is_empty() || quoted {
                tokens.push(std::mem::take(&mut current));
                quoted = false;
            }
        } else if char == '\\' {
            match chars.get(index + 1) {
                Some(&next) if is_js_whitespace(next) => {
                    current.push(next);
                    index += 1;
                }
                _ => current.push(char),
            }
        } else {
            current.push(char);
        }
        index += 1;
    }
    if !current.is_empty() || quoted {
        tokens.push(current);
    }
    tokens
}

/// JavaScript's `\s`.
fn is_js_whitespace(char: char) -> bool {
    char.is_whitespace() || char == '\u{feff}'
}

/// `codexAppServerArgs`: `app-server` followed by the launch args.
pub fn codex_app_server_args(launch_args: Option<&str>) -> Vec<String> {
    let mut args = vec!["app-server".to_owned()];
    args.extend(tokenize_cli_args(launch_args));
    args
}

/// `codexExecLaunchArgs`: the subset of launch args `codex exec` shares with `app-server`.
pub fn codex_exec_launch_args(launch_args: Option<&str>) -> Vec<String> {
    let args = tokenize_cli_args(launch_args);
    let mut exec_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--strict-config" || arg.starts_with("--config=") || arg.starts_with("-c=") {
            exec_args.push(arg.clone());
        } else if matches!(arg.as_str(), "--config" | "-c" | "--enable" | "--disable") {
            if let Some(value) = args.get(index + 1) {
                if !value.starts_with('-') {
                    exec_args.push(arg.clone());
                    exec_args.push(value.clone());
                    index += 1;
                }
            }
        } else if arg.starts_with("--enable=") || arg.starts_with("--disable=") {
            exec_args.push(arg.clone());
        }
        index += 1;
    }
    exec_args
}

/// `codexSessionAppServerArgs`: the launch args, then the session's own `-c` flags.
pub fn codex_session_app_server_args(app_server_args: Option<&[String]>, launch_args: Option<&str>) -> Vec<String> {
    let mut args = codex_app_server_args(launch_args);
    if let Some(extra) = app_server_args {
        args.extend(extra.iter().cloned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
    }

    #[test]
    fn launch_args_env_wins_over_settings() {
        assert_eq!(
            resolve_codex_launch_args(Some(" --strict-config "), &env(&[("T3CODE_CODEX_LAUNCH_ARGS", "--enable foo")])),
            "--enable foo"
        );
    }

    #[test]
    fn settings_apply_when_the_env_value_is_blank() {
        assert_eq!(
            resolve_codex_launch_args(Some(" --strict-config "), &env(&[("T3CODE_CODEX_LAUNCH_ARGS", "   ")])),
            "--strict-config"
        );
        assert_eq!(resolve_codex_launch_args(Some(""), &env(&[("T3CODE_CODEX_LAUNCH_ARGS", "   ")])), "");
    }

    #[test]
    fn app_server_args() {
        assert_eq!(codex_app_server_args(Some("")), vec!["app-server"]);
        assert_eq!(
            codex_app_server_args(Some("--strict-config --enable foo")),
            vec!["app-server", "--strict-config", "--enable", "foo"]
        );
    }

    #[test]
    fn exec_args_keep_shared_flags_only() {
        assert_eq!(
            codex_exec_launch_args(Some("--strict-config --enable foo --listen off --config model=\"gpt 5\"")),
            vec!["--strict-config", "--enable", "foo", "--config", "model=gpt 5"]
        );
        assert_eq!(
            codex_exec_launch_args(Some("--config --strict-config --enable --disable")),
            vec!["--strict-config"]
        );
    }

    #[test]
    fn session_args_keep_launch_args_before_explicit_ones() {
        assert_eq!(
            codex_session_app_server_args(Some(&["-c".to_owned(), "model=gpt-5".to_owned()]), None),
            vec!["app-server", "-c", "model=gpt-5"]
        );
        assert_eq!(
            codex_session_app_server_args(
                Some(&["-c".to_owned(), "mcp_servers.t3-code.url=http://127.0.0.1/mcp".to_owned()]),
                Some("--strict-config --enable foo")
            ),
            vec![
                "app-server",
                "--strict-config",
                "--enable",
                "foo",
                "-c",
                "mcp_servers.t3-code.url=http://127.0.0.1/mcp"
            ]
        );
    }

    #[test]
    fn tokenizer_quotes_and_escapes() {
        assert_eq!(
            tokenize_cli_args(Some("-c 'a b' \"c \\\"d\\\"\" e\\ f ''")),
            vec!["-c", "a b", "c \"d\"", "e f", ""]
        );
        assert_eq!(tokenize_cli_args(Some("  ")), Vec::<String>::new());
        assert_eq!(tokenize_cli_args(None), Vec::<String>::new());
        assert_eq!(tokenize_cli_args(Some("\"a\\nb\"")), vec!["a\\nb"]);
    }
}
