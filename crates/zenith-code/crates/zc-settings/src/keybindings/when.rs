//! The keybinding `when` expression language (`packages/shared/src/keybindings.ts`
//! `parseKeybindingWhenExpression`), and its encoder (`keybindings.ts` `encodeWhenAst`).
//!
//! Grammar: identifiers (`[A-Za-z_][A-Za-z0-9_.-]*`), `!`, `&&`, `||` and parentheses, with the
//! usual precedence (`!` > `&&` > `||`, left-associative). Nesting (parentheses) and runs of `!`
//! are limited to [`MAX_WHEN_EXPRESSION_DEPTH`].

use zc_contracts::{
    KeybindingWhenNode, KeybindingWhenNodeAnd, KeybindingWhenNodeIdentifier, KeybindingWhenNodeNot, KeybindingWhenNodeOr, LitAnd, LitIdentifier, LitNot, LitOr,
};

/// `MAX_WHEN_EXPRESSION_DEPTH`.
pub const MAX_WHEN_EXPRESSION_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Identifier(String),
    Not,
    And,
    Or,
    LParen,
    RParen,
}

/// JS `\s`.
fn is_js_space(ch: char) -> bool {
    crate::js::is_js_whitespace(ch)
}

fn tokenize(expression: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut rest = expression;
    while let Some(current) = rest.chars().next() {
        if is_js_space(current) {
            rest = &rest[current.len_utf8()..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("&&") {
            tokens.push(Token::And);
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("||") {
            tokens.push(Token::Or);
            rest = after;
            continue;
        }
        let single = match current {
            '!' => Some(Token::Not),
            '(' => Some(Token::LParen),
            ')' => Some(Token::RParen),
            _ => None,
        };
        if let Some(token) = single {
            tokens.push(token);
            rest = &rest[1..];
            continue;
        }
        // /^[A-Za-z_][A-Za-z0-9_.-]*/
        if !(current.is_ascii_alphabetic() || current == '_') {
            return None;
        }
        let length = rest
            .char_indices()
            .find(|(index, ch)| *index > 0 && !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')))
            .map_or(rest.len(), |(index, _)| index);
        tokens.push(Token::Identifier(rest[..length].to_owned()));
        rest = &rest[length..];
    }
    Some(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn primary(&mut self, depth: usize) -> Option<KeybindingWhenNode> {
        if depth > MAX_WHEN_EXPRESSION_DEPTH {
            return None;
        }
        match self.peek()?.clone() {
            Token::Identifier(name) => {
                self.index += 1;
                Some(identifier(name))
            }
            Token::LParen => {
                self.index += 1;
                let node = self.or(depth + 1)?;
                if self.peek() != Some(&Token::RParen) {
                    return None;
                }
                self.index += 1;
                Some(node)
            }
            _ => None,
        }
    }

    fn unary(&mut self, depth: usize) -> Option<KeybindingWhenNode> {
        let mut not_count = 0;
        while self.peek() == Some(&Token::Not) {
            self.index += 1;
            not_count += 1;
            if not_count > MAX_WHEN_EXPRESSION_DEPTH {
                return None;
            }
        }
        let mut node = self.primary(depth)?;
        for _ in 0..not_count {
            node = KeybindingWhenNode::Not(KeybindingWhenNodeNot {
                r#type: LitNot,
                node: Box::new(node),
            });
        }
        Some(node)
    }

    fn and(&mut self, depth: usize) -> Option<KeybindingWhenNode> {
        let mut left = self.unary(depth)?;
        while self.peek() == Some(&Token::And) {
            self.index += 1;
            let right = self.unary(depth)?;
            left = KeybindingWhenNode::And(KeybindingWhenNodeAnd {
                r#type: LitAnd,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Some(left)
    }

    fn or(&mut self, depth: usize) -> Option<KeybindingWhenNode> {
        let mut left = self.and(depth)?;
        while self.peek() == Some(&Token::Or) {
            self.index += 1;
            let right = self.and(depth)?;
            left = KeybindingWhenNode::Or(KeybindingWhenNodeOr {
                r#type: LitOr,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Some(left)
    }
}

fn identifier(name: String) -> KeybindingWhenNode {
    KeybindingWhenNode::Identifier(KeybindingWhenNodeIdentifier { r#type: LitIdentifier, name })
}

/// `parseKeybindingWhenExpression`: the AST, or `None` for anything malformed.
pub fn parse_keybinding_when_expression(expression: &str) -> Option<KeybindingWhenNode> {
    let tokens = tokenize(expression)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = Parser { tokens, index: 0 };
    let ast = parser.or(0)?;
    (parser.index == parser.tokens.len()).then_some(ast)
}

/// `encodeWhenAst`: fully parenthesized text that parses back to the same AST.
pub fn encode_when_ast(node: &KeybindingWhenNode) -> String {
    match node {
        KeybindingWhenNode::Identifier(identifier) => identifier.name.clone(),
        KeybindingWhenNode::Not(not) => format!("!({})", encode_when_ast(&not.node)),
        KeybindingWhenNode::And(and) => format!("({} && {})", encode_when_ast(&and.left), encode_when_ast(&and.right)),
        KeybindingWhenNode::Or(or) => format!("({} || {})", encode_when_ast(&or.left), encode_when_ast(&or.right)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn ast(expression: &str) -> Option<Value> {
        parse_keybinding_when_expression(expression).map(|node| serde_json::to_value(node).unwrap())
    }

    fn id(name: &str) -> Value {
        json!({"type": "identifier", "name": name})
    }

    fn not(node: Value) -> Value {
        json!({"type": "not", "node": node})
    }

    fn and(left: Value, right: Value) -> Value {
        json!({"type": "and", "left": left, "right": right})
    }

    fn or(left: Value, right: Value) -> Value {
        json!({"type": "or", "left": left, "right": right})
    }

    #[test]
    fn parses_identifiers_and_negation() {
        assert_eq!(ast("terminalFocus"), Some(id("terminalFocus")));
        assert_eq!(ast("!terminalFocus"), Some(not(id("terminalFocus"))));
        assert_eq!(ast("!!a"), Some(not(not(id("a")))));
        assert_eq!(ast("a.b-c_d"), Some(id("a.b-c_d")));
        assert_eq!(ast("_x"), Some(id("_x")));
    }

    #[test]
    fn respects_precedence_and_associativity() {
        // `compiles valid rule with parsed when AST` (keybindings.test.ts).
        assert_eq!(ast("terminalOpen && !terminalFocus"), Some(and(id("terminalOpen"), not(id("terminalFocus")))));
        assert_eq!(ast("a || b && c"), Some(or(id("a"), and(id("b"), id("c")))));
        assert_eq!(ast("a && b && c"), Some(and(and(id("a"), id("b")), id("c"))));
        assert_eq!(ast("a || b || c"), Some(or(or(id("a"), id("b")), id("c"))));
        assert_eq!(ast("(a || b) && c"), Some(and(or(id("a"), id("b")), id("c"))));
        assert_eq!(ast("!(a && b)"), Some(not(and(id("a"), id("b")))));
        assert_eq!(ast("  modelPickerOpen  &&\tisDesktop "), Some(and(id("modelPickerOpen"), id("isDesktop"))));
    }

    #[test]
    fn rejects_malformed_expressions() {
        for expression in [
            "",
            "   ",
            "a &&",
            "&& a",
            "a b",
            "(a",
            "a)",
            "()",
            "a & b",
            "a | b",
            "1a",
            "a == b",
            "!",
            "a || (b && )",
        ] {
            assert_eq!(ast(expression), None, "{expression:?}");
        }
    }

    #[test]
    fn limits_nesting_and_negation_depth() {
        let nested_ok = format!("{}a{}", "(".repeat(64), ")".repeat(64));
        assert!(ast(&nested_ok).is_some());
        let nested_too_deep = format!("{}a{}", "(".repeat(65), ")".repeat(65));
        assert_eq!(ast(&nested_too_deep), None);
        assert!(ast(&format!("{}a", "!".repeat(64))).is_some());
        assert_eq!(ast(&format!("{}a", "!".repeat(65))), None);
    }

    #[test]
    fn encodes_back_to_an_equivalent_expression() {
        for expression in ["a", "!a", "a && !b", "(a || b) && c", "!(a && (b || !c))"] {
            let node = parse_keybinding_when_expression(expression).unwrap();
            let encoded = encode_when_ast(&node);
            assert_eq!(parse_keybinding_when_expression(&encoded), Some(node), "{encoded}");
        }
        assert_eq!(
            encode_when_ast(&parse_keybinding_when_expression("a && !b || c").unwrap()),
            "((a && !(b)) || c)"
        );
    }
}
