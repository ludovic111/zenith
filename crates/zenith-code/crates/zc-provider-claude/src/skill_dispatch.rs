//! `provider/Drivers/ClaudeSkillDispatch.ts`: a `$skill` mention becomes the `/skill` slash
//! command Claude Code runs. Claude Code only expands a command from the message's last text
//! block, when `/name` is its first character, and only one per message; so the last mention
//! moves to a trailing block and earlier ones are rewritten to `/name` inline.
//!
//! The TS pattern relies on lookarounds the `regex` crate lacks, so the scan below reproduces
//! `matchAll` of
//! `/(^|\s)\p{Sc}(?![0-9][0-9_]*(?:[kKmMbBtT]|[eE][0-9]+)?(?:\s|$))(?=[a-zA-Z0-9:_-]*[a-zA-Z])([a-zA-Z0-9][a-zA-Z0-9:_-]*)(?=\s|$)/gu`
//! by hand.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::cli_args::is_js_space;

/// `ClaudeSkillDispatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSkillDispatch {
    /// Text before the dispatched mention, `None` when it opens the prompt.
    pub leading_text: Option<String>,
    /// `/name` plus the trailing text: the message's last text block.
    pub command_text: String,
    pub skill_name: String,
}

fn is_currency_symbol(c: char) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\p{Sc}$").expect("valid regex"));
    let mut buffer = [0u8; 4];
    re.is_match(c.encode_utf8(&mut buffer))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-')
}

fn at_boundary(chars: &[(usize, char)], index: usize) -> bool {
    chars.get(index).is_none_or(|(_, c)| is_js_space(*c))
}

/// The negative lookahead: a currency amount (`$20`, `$20k`, `$1e6`) right after the symbol.
fn is_amount(chars: &[(usize, char)], start: usize) -> bool {
    if !chars.get(start).is_some_and(|(_, c)| c.is_ascii_digit()) {
        return false;
    }
    let mut run_end = start + 1;
    while chars.get(run_end).is_some_and(|(_, c)| c.is_ascii_digit() || *c == '_') {
        run_end += 1;
    }
    (start + 1..=run_end).rev().any(|end| {
        if at_boundary(chars, end) {
            return true;
        }
        match chars.get(end).map(|(_, c)| *c) {
            Some('k' | 'K' | 'm' | 'M' | 'b' | 'B' | 't' | 'T') => at_boundary(chars, end + 1),
            Some('e' | 'E') => {
                let mut digits_end = end + 1;
                while chars.get(digits_end).is_some_and(|(_, c)| c.is_ascii_digit()) {
                    digits_end += 1;
                }
                digits_end > end + 1 && at_boundary(chars, digits_end)
            }
            _ => false,
        }
    })
}

/// One match: (byte start of the symbol, byte end of the name, name).
fn match_at(chars: &[(usize, char)], text: &str, symbol_index: usize) -> Option<(usize, usize, String)> {
    let (symbol_byte, symbol) = *chars.get(symbol_index)?;
    if !is_currency_symbol(symbol) {
        return None;
    }
    let name_start = symbol_index + 1;
    if is_amount(chars, name_start) {
        return None;
    }
    let mut run_end = name_start;
    while chars.get(run_end).is_some_and(|(_, c)| is_name_char(*c)) {
        run_end += 1;
    }
    if !chars[name_start..run_end].iter().any(|(_, c)| c.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.get(name_start).is_some_and(|(_, c)| c.is_ascii_alphanumeric()) || !at_boundary(chars, run_end) {
        return None;
    }
    let name_byte_start = chars[name_start].0;
    let name_byte_end = chars.get(run_end).map_or(text.len(), |(byte, _)| *byte);
    Some((symbol_byte, name_byte_end, text[name_byte_start..name_byte_end].to_string()))
}

/// Every mention, as `matchAll` would find them (non-overlapping, left to right).
fn find_mentions(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut mentions = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        // `(^|\s)`: the empty start-of-input alternative first, then one whitespace character.
        let found = if index == 0 { match_at(&chars, text, 0) } else { None }.map(|m| (m, 0)).or_else(|| {
            if is_js_space(chars[index].1) {
                match_at(&chars, text, index + 1).map(|m| (m, 1))
            } else {
                None
            }
        });
        match found {
            Some(((start, end, name), _)) => {
                mentions.push((start, end, name));
                // Resume right after the match.
                index = chars.iter().position(|(byte, _)| *byte >= end).unwrap_or(chars.len());
            }
            None => index += 1,
        }
    }
    mentions
}

/// `planClaudeSkillDispatch(prompt, skillNames)`: `None` when no mention names a known skill.
pub fn plan_claude_skill_dispatch(prompt: &str, skill_names: &HashSet<String>) -> Option<ClaudeSkillDispatch> {
    let mentions: Vec<(usize, usize, String)> = find_mentions(prompt).into_iter().filter(|(_, _, name)| skill_names.contains(name)).collect();
    let (last_start, last_end, last_name) = mentions.last()?.clone();
    let mut leading = prompt[..last_start].to_string();
    for (start, end, name) in mentions[..mentions.len() - 1].iter().rev() {
        leading = format!("{}/{name}{}", &leading[..*start], &leading[*end..]);
    }
    let leading = leading.trim_end_matches(is_js_space).to_string();
    let command_text = format!("/{last_name}{}", &prompt[last_end..]).trim_end_matches(is_js_space).to_string();
    Some(ClaudeSkillDispatch {
        leading_text: (!leading.is_empty()).then_some(leading),
        command_text,
        skill_name: last_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills() -> HashSet<String> {
        ["2spec", "implement", "review", "re-release-version"].iter().map(|s| s.to_string()).collect()
    }

    fn plan(leading: Option<&str>, command: &str, name: &str) -> Option<ClaudeSkillDispatch> {
        Some(ClaudeSkillDispatch {
            leading_text: leading.map(str::to_string),
            command_text: command.into(),
            skill_name: name.into(),
        })
    }

    #[test]
    fn leaves_a_prompt_without_a_known_skill_untouched() {
        assert_eq!(plan_claude_skill_dispatch("fix the build", &skills()), None);
        assert_eq!(plan_claude_skill_dispatch("echo $HOME then $unknown", &skills()), None);
    }

    #[test]
    fn moves_a_mid_prompt_mention_into_a_trailing_slash_command() {
        assert_eq!(
            plan_claude_skill_dispatch("ok, now $implement all the tickets", &skills()),
            plan(Some("ok, now"), "/implement all the tickets", "implement")
        );
    }

    #[test]
    fn keeps_a_mention_that_opens_the_prompt_as_a_single_command_block() {
        assert_eq!(
            plan_claude_skill_dispatch("$review\nfocus on auth", &skills()),
            plan(None, "/review\nfocus on auth", "review")
        );
    }

    #[test]
    fn dispatches_a_known_skill_whose_name_begins_with_a_digit() {
        assert_eq!(
            plan_claude_skill_dispatch("use $2spec for this", &skills()),
            plan(Some("use"), "/2spec for this", "2spec")
        );
    }

    #[test]
    fn dispatches_the_last_mention_and_rewrites_earlier_ones_inline() {
        assert_eq!(
            plan_claude_skill_dispatch("$review the diff, then $implement the fixes", &skills()),
            plan(Some("/review the diff, then"), "/implement the fixes", "implement")
        );
    }

    #[test]
    fn dispatches_currency_prefixed_mentions_and_preserves_their_source_boundaries() {
        for symbol in ["€", "£", "¥", "₹", "₩", "₿", "𑿝"] {
            assert_eq!(
                plan_claude_skill_dispatch(&format!("{symbol}review the diff, then {symbol}implement the fixes"), &skills()),
                plan(Some("/review the diff, then"), "/implement the fixes", "implement")
            );
            assert_eq!(
                plan_claude_skill_dispatch(&format!("{symbol}2spec for this"), &skills()),
                plan(None, "/2spec for this", "2spec")
            );
            assert_eq!(plan_claude_skill_dispatch(&format!("5{symbol}review {symbol}unknown"), &skills()), None);
        }
    }

    #[test]
    fn ignores_a_dollar_token_glued_to_other_text() {
        assert_eq!(plan_claude_skill_dispatch("cost is 5$implement", &skills()), None);
    }

    #[test]
    fn ignores_currency_amounts_and_compact_monetary_expressions() {
        let mut with_currency = skills();
        with_currency.extend(["20", "20k", "100M", "1e6"].iter().map(|s| s.to_string()));
        for symbol in ["$", "€", "£", "¥", "₹", "₩", "₿", "𑿝"] {
            let prompt = format!("pay {symbol}20 {symbol}20k {symbol}100M {symbol}1e6 tomorrow");
            assert_eq!(plan_claude_skill_dispatch(&prompt, &with_currency), None);
        }
    }

    #[test]
    fn keeps_trailing_newlines_inside_the_command() {
        assert_eq!(
            plan_claude_skill_dispatch("ok, now $implement all the tickets\nstart with auth", &skills()),
            plan(Some("ok, now"), "/implement all the tickets\nstart with auth", "implement")
        );
    }
}
