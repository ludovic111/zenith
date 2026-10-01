//! `packages/shared/src/cliArgs.ts`: the quote-aware tokenizer and flag parser used on the
//! instance's `launchArgs`.
//!
//! Flags are kept in a `serde_json::Map` (insertion-ordered, `null` for a bare flag) because
//! that is exactly the JS object the adapter hands the SDK as `extraArgs`: re-assigning a key
//! keeps its first position, like a JS property.

use serde_json::{Map, Value};

/// JS `\s` for one character (WhiteSpace + LineTerminator).
pub(crate) fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `tokenizeCliArgs`: split on unquoted whitespace; `'…'` and `"…"` group, `\` escapes a
/// following whitespace outside quotes and `" \ $ \`` inside double quotes.
pub fn tokenize_cli_args(args: &str) -> Vec<String> {
    let input = args.trim_matches(is_js_space);
    if input.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if let Some(q) = quote {
            if c == q {
                quote = None;
                quoted = true;
            } else if c == '\\' && q == '"' {
                match chars.get(index + 1) {
                    Some(&next) if matches!(next, '"' | '\\' | '$' | '`') => {
                        current.push(next);
                        index += 1;
                    }
                    _ => current.push(c),
                }
            } else {
                current.push(c);
            }
            index += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            quoted = true;
        } else if is_js_space(c) {
            if !current.is_empty() || quoted {
                tokens.push(std::mem::take(&mut current));
                quoted = false;
            }
        } else if c == '\\' {
            match chars.get(index + 1) {
                Some(&next) if is_js_space(next) => {
                    current.push(next);
                    index += 1;
                }
                _ => current.push(c),
            }
        } else {
            current.push(c);
        }
        index += 1;
    }
    if !current.is_empty() || quoted {
        tokens.push(current);
    }
    tokens
}

/// `parseCliArgs(...)` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedCliArgs {
    /// Flag name → value (`Value::String`) or `Value::Null` for a bare flag, in first-seen order.
    pub flags: Map<String, Value>,
    pub positionals: Vec<String>,
}

impl ParsedCliArgs {
    /// `flags[name]`: `None` when absent, `Some(None)` for a bare flag.
    pub fn flag(&self, name: &str) -> Option<Option<&str>> {
        self.flags.get(name).map(Value::as_str)
    }
}

/// `parseCliArgs(args)` over a launch-args string.
pub fn parse_cli_args(args: &str) -> ParsedCliArgs {
    parse_cli_tokens(&tokenize_cli_args(args), &[])
}

/// `parseCliArgs(tokens, {booleanFlags})`.
pub fn parse_cli_tokens(tokens: &[String], boolean_flags: &[&str]) -> ParsedCliArgs {
    let mut parsed = ParsedCliArgs::default();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if let Some(rest) = token.strip_prefix("--") {
            if rest.is_empty() {
                index += 1;
                continue;
            }
            if let Some(eq) = rest.find('=') {
                parsed.flags.insert(rest[..eq].to_string(), Value::String(rest[eq + 1..].to_string()));
                index += 1;
                continue;
            }
            if boolean_flags.contains(&rest) {
                parsed.flags.insert(rest.to_string(), Value::Null);
                index += 1;
                continue;
            }
            match tokens.get(index + 1) {
                Some(next) if !next.starts_with("--") => {
                    parsed.flags.insert(rest.to_string(), Value::String(next.clone()));
                    index += 2;
                }
                _ => {
                    parsed.flags.insert(rest.to_string(), Value::Null);
                    index += 1;
                }
            }
        } else {
            parsed.positionals.push(token.clone());
            index += 1;
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_documented_examples() {
        assert_eq!(parse_cli_args("").flags, Map::new());
        assert_eq!(Value::Object(parse_cli_args("--chrome").flags), json!({"chrome": null}));
        assert_eq!(
            Value::Object(parse_cli_args("--chrome --effort high").flags),
            json!({"chrome": null, "effort": "high"})
        );
        assert_eq!(Value::Object(parse_cli_args("--effort=high").flags), json!({"effort": "high"}));
        let tokens: Vec<String> = ["1.2.3", "--root", "/path", "--github-output"].iter().map(|s| s.to_string()).collect();
        let parsed = parse_cli_tokens(&tokens, &["github-output"]);
        assert_eq!(Value::Object(parsed.flags), json!({"root": "/path", "github-output": null}));
        assert_eq!(parsed.positionals, vec!["1.2.3"]);
    }

    #[test]
    fn tokenizes_quotes_and_escapes() {
        assert_eq!(tokenize_cli_args(r#"--a "b c" 'd e' f\ g"#), vec!["--a", "b c", "d e", "f g"]);
        assert_eq!(tokenize_cli_args(r#"--x "a\"b\\c\$d" """#), vec!["--x", "a\"b\\c$d", ""]);
        assert_eq!(tokenize_cli_args("  "), Vec::<String>::new());
    }

    #[test]
    fn reassigning_a_flag_keeps_its_first_position() {
        let parsed = parse_cli_args("--a 1 --b --a 2");
        assert_eq!(parsed.flags.keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(parsed.flag("a"), Some(Some("2")));
        assert_eq!(parsed.flag("b"), Some(None));
    }
}
