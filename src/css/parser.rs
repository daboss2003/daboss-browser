//! CSS parser. Recursive-descent over the raw character stream — no separate
//! token type. Skips `@`-rules entirely, ignores `!important`, ignores
//! pseudo-classes and pseudo-elements (parses them so the surrounding selector
//! still works, then discards). Good enough for ~90% of real-world CSS.

use super::types::{
    Color, Combinator, Declaration, Rule, Selector, SimpleSelector, Stylesheet, Unit, Value,
};

pub fn parse(input: &str) -> Stylesheet {
    Parser::new(input).parse_stylesheet()
}

pub fn parse_inline_declarations(input: &str) -> Vec<Declaration> {
    let mut p = Parser::new(input);
    p.parse_declarations()
}

pub struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse_stylesheet(&mut self) -> Stylesheet {
        let mut rules = Vec::new();
        loop {
            self.skip_ws();
            if self.eof() {
                break;
            }
            if self.peek() == Some('@') {
                self.skip_at_rule();
                continue;
            }
            if self.peek() == Some('}') {
                self.advance();
                continue;
            }
            if let Some(rule) = self.parse_rule() {
                rules.push(rule);
            } else {
                // Recover: skip to next '}' or '{' so we don't loop forever.
                self.skip_to_block_end_or_semi();
            }
        }
        Stylesheet { rules }
    }

    fn parse_rule(&mut self) -> Option<Rule> {
        let selectors = self.parse_selector_list()?;
        self.skip_ws();
        if !self.consume('{') {
            return None;
        }
        let declarations = self.parse_declarations();
        self.skip_ws();
        self.consume('}');
        Some(Rule {
            selectors,
            declarations,
        })
    }

    fn parse_selector_list(&mut self) -> Option<Vec<Selector>> {
        let mut sels = Vec::new();
        loop {
            self.skip_ws();
            let sel = self.parse_selector()?;
            sels.push(sel);
            self.skip_ws();
            if self.peek() == Some(',') {
                self.advance();
                continue;
            }
            break;
        }
        if sels.is_empty() {
            None
        } else {
            Some(sels)
        }
    }

    fn parse_selector(&mut self) -> Option<Selector> {
        let mut compounds = Vec::new();
        let mut combinators = Vec::new();

        let first = self.parse_compound()?;
        compounds.push(first);

        loop {
            let had_ws = self.skip_ws_count() > 0;
            let combinator = match self.peek() {
                Some('>') => {
                    self.advance();
                    Combinator::Child
                }
                Some('+') => {
                    self.advance();
                    Combinator::AdjacentSibling
                }
                Some('~') => {
                    self.advance();
                    Combinator::GeneralSibling
                }
                Some('{') | Some(',') | None => break,
                _ if had_ws => Combinator::Descendant,
                _ => break,
            };
            self.skip_ws();
            if let Some(next) = self.parse_compound() {
                combinators.push(combinator);
                compounds.push(next);
            } else {
                break;
            }
        }

        Some(Selector {
            compounds,
            combinators,
        })
    }

    fn parse_compound(&mut self) -> Option<SimpleSelector> {
        let mut ss = SimpleSelector::default();
        let mut had = false;
        loop {
            match self.peek() {
                Some('#') => {
                    self.advance();
                    let name = self.parse_ident()?;
                    ss.id = Some(name);
                    had = true;
                }
                Some('.') => {
                    self.advance();
                    let name = self.parse_ident()?;
                    ss.classes.push(name);
                    had = true;
                }
                Some('*') => {
                    self.advance();
                    had = true; // universal
                }
                Some(':') => {
                    self.advance();
                    // Pseudo-element uses `::` — eat the second colon
                    if self.peek() == Some(':') {
                        self.advance();
                    }
                    // Skip the pseudo name + any parenthesized argument
                    let _ = self.parse_ident();
                    if self.peek() == Some('(') {
                        self.skip_balanced('(', ')');
                    }
                    had = true;
                }
                Some('[') => {
                    // Attribute selector. We don't support them; consume so it
                    // doesn't confuse the outer parser.
                    self.skip_balanced('[', ']');
                    had = true;
                }
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                    let tag = self.parse_ident()?;
                    ss.tag = Some(tag);
                    had = true;
                }
                _ => break,
            }
        }
        if had {
            Some(ss)
        } else {
            None
        }
    }

    fn parse_declarations(&mut self) -> Vec<Declaration> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None | Some('}') => break,
                Some(';') => {
                    self.advance();
                    continue;
                }
                _ => {}
            }
            if let Some(d) = self.parse_declaration() {
                out.push(d);
            } else {
                // Skip to ';' or '}'
                while let Some(c) = self.peek() {
                    if c == ';' || c == '}' {
                        break;
                    }
                    self.advance();
                }
            }
        }
        out
    }

    fn parse_declaration(&mut self) -> Option<Declaration> {
        let property = self.parse_ident()?.to_ascii_lowercase();
        self.skip_ws();
        if !self.consume(':') {
            return None;
        }
        let value = self.parse_value_list();
        // Discard '!important' marker.
        self.skip_ws();
        if self.peek() == Some('!') {
            self.advance();
            let _ = self.parse_ident();
        }
        Some(Declaration { property, value })
    }

    fn parse_value_list(&mut self) -> Value {
        let mut values = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None | Some(';') | Some('}') | Some('!') => break,
                Some(',') => {
                    self.advance();
                    continue;
                }
                _ => {}
            }
            if let Some(v) = self.parse_one_value() {
                values.push(v);
            } else {
                break;
            }
        }
        match values.len() {
            0 => Value::Keyword(String::new()),
            1 => values.into_iter().next().unwrap(),
            _ => Value::List(values),
        }
    }

    fn parse_one_value(&mut self) -> Option<Value> {
        let c = self.peek()?;
        match c {
            '"' | '\'' => self.parse_string_value(),
            '#' => self.parse_hex_color(),
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.parse_numeric(),
            c if c.is_ascii_alphabetic() || c == '_' => self.parse_ident_or_function(),
            _ => None,
        }
    }

    fn parse_string_value(&mut self) -> Option<Value> {
        let quote = self.advance()?;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == quote {
                self.advance();
                return Some(Value::String(s));
            }
            if c == '\\' {
                self.advance();
                if let Some(esc) = self.advance() {
                    s.push(esc);
                }
                continue;
            }
            s.push(c);
            self.advance();
        }
        Some(Value::String(s))
    }

    fn parse_hex_color(&mut self) -> Option<Value> {
        self.advance(); // '#'
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_hexdigit() {
                self.advance();
            } else {
                break;
            }
        }
        let hex = &self.input[start..self.pos];
        Some(Value::Color(parse_hex(hex)?))
    }

    fn parse_numeric(&mut self) -> Option<Value> {
        let start = self.pos;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.advance();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let num_str = &self.input[start..self.pos];
        let n: f32 = num_str.parse().ok()?;

        // Unit or percentage
        if self.peek() == Some('%') {
            self.advance();
            return Some(Value::Percentage(n));
        }
        let unit_start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.advance();
            } else {
                break;
            }
        }
        if unit_start == self.pos {
            return Some(Value::Number(n));
        }
        let unit_str = &self.input[unit_start..self.pos];
        let unit = match unit_str.to_ascii_lowercase().as_str() {
            "px" => Unit::Px,
            "em" => Unit::Em,
            "rem" => Unit::Rem,
            "pt" => Unit::Pt,
            "pc" => Unit::Pc,
            "cm" => Unit::Cm,
            "mm" => Unit::Mm,
            "in" => Unit::In,
            "vw" => Unit::Vw,
            "vh" => Unit::Vh,
            _ => return Some(Value::Number(n)), // unknown unit
        };
        Some(Value::Length(n, unit))
    }

    fn parse_ident_or_function(&mut self) -> Option<Value> {
        let name = self.parse_ident()?;
        if self.peek() == Some('(') {
            self.advance();
            let args = self.parse_function_args();
            self.consume(')');
            return Some(function_value(&name, args));
        }
        // Named color?
        if let Some(c) = named_color(&name) {
            return Some(Value::Color(c));
        }
        Some(Value::Keyword(name.to_ascii_lowercase()))
    }

    fn parse_function_args(&mut self) -> Vec<Value> {
        let mut args = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(')') || self.eof() {
                break;
            }
            if self.peek() == Some(',') {
                self.advance();
                continue;
            }
            if let Some(v) = self.parse_one_value() {
                args.push(v);
            } else {
                self.advance();
            }
        }
        args
    }

    fn parse_ident(&mut self) -> Option<String> {
        let start = self.pos;
        // First char: letter, underscore, or '-' followed by letter/underscore
        let first = self.peek()?;
        if first == '-' {
            self.advance();
            let next = self.peek()?;
            if !(next.is_ascii_alphabetic() || next == '_') {
                self.pos = start;
                return None;
            }
        } else if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        } else {
            self.advance();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                self.advance();
            } else {
                break;
            }
        }
        Some(self.input[start..self.pos].to_string())
    }

    fn skip_at_rule(&mut self) {
        // Skip @ident; consume balanced block or up to ';'.
        self.advance(); // '@'
        let _ = self.parse_ident();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return,
                Some(';') => {
                    self.advance();
                    return;
                }
                Some('{') => {
                    self.skip_balanced('{', '}');
                    return;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn skip_to_block_end_or_semi(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                ';' => {
                    self.advance();
                    return;
                }
                '{' => {
                    self.skip_balanced('{', '}');
                    return;
                }
                '}' => return,
                _ => self.advance(),
            };
        }
    }

    fn skip_balanced(&mut self, open: char, close: char) {
        if self.peek() != Some(open) {
            return;
        }
        self.advance();
        let mut depth = 1;
        while let Some(c) = self.peek() {
            self.advance();
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if c == '"' || c == '\'' {
                // Skip string content so braces inside strings don't confuse us.
                while let Some(s) = self.peek() {
                    self.advance();
                    if s == c {
                        break;
                    }
                    if s == '\\' {
                        self.advance();
                    }
                }
            }
        }
    }

    fn skip_ws(&mut self) {
        self.skip_ws_count();
    }

    fn skip_ws_count(&mut self) -> usize {
        let start = self.pos;
        loop {
            // Whitespace
            while let Some(c) = self.peek() {
                if c.is_ascii_whitespace() {
                    self.advance();
                } else {
                    break;
                }
            }
            // Comment
            if self.input[self.pos..].starts_with("/*") {
                self.pos += 2;
                while !self.input[self.pos..].starts_with("*/") && !self.eof() {
                    self.pos += 1;
                }
                if self.input[self.pos..].starts_with("*/") {
                    self.pos += 2;
                }
                continue;
            }
            break;
        }
        self.pos - start
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn consume(&mut self, want: char) -> bool {
        if self.peek() == Some(want) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }
}

fn function_value(name: &str, args: Vec<Value>) -> Value {
    match name.to_ascii_lowercase().as_str() {
        "rgb" | "rgba" => {
            if args.len() >= 3 {
                let r = number_to_byte(&args[0]);
                let g = number_to_byte(&args[1]);
                let b = number_to_byte(&args[2]);
                let a = if args.len() >= 4 {
                    match &args[3] {
                        Value::Number(n) => (n * 255.0).clamp(0.0, 255.0) as u8,
                        _ => 255,
                    }
                } else {
                    255
                };
                return Value::Color(Color { r, g, b, a });
            }
            Value::Keyword(name.to_string())
        }
        _ => Value::Keyword(name.to_string()),
    }
}

fn number_to_byte(v: &Value) -> u8 {
    match v {
        Value::Number(n) => n.round().clamp(0.0, 255.0) as u8,
        Value::Percentage(p) => (p * 2.55).round().clamp(0.0, 255.0) as u8,
        _ => 0,
    }
}

fn parse_hex(hex: &str) -> Option<Color> {
    let parse = |s: &str| -> Option<u8> { u8::from_str_radix(s, 16).ok() };
    match hex.len() {
        3 => {
            let r = parse(&hex[0..1])? * 17;
            let g = parse(&hex[1..2])? * 17;
            let b = parse(&hex[2..3])? * 17;
            Some(Color { r, g, b, a: 255 })
        }
        4 => {
            let r = parse(&hex[0..1])? * 17;
            let g = parse(&hex[1..2])? * 17;
            let b = parse(&hex[2..3])? * 17;
            let a = parse(&hex[3..4])? * 17;
            Some(Color { r, g, b, a })
        }
        6 => {
            let r = parse(&hex[0..2])?;
            let g = parse(&hex[2..4])?;
            let b = parse(&hex[4..6])?;
            Some(Color { r, g, b, a: 255 })
        }
        8 => {
            let r = parse(&hex[0..2])?;
            let g = parse(&hex[2..4])?;
            let b = parse(&hex[4..6])?;
            let a = parse(&hex[6..8])?;
            Some(Color { r, g, b, a })
        }
        _ => None,
    }
}

fn named_color(name: &str) -> Option<Color> {
    let n = name.to_ascii_lowercase();
    let c = match n.as_str() {
        "black" => Color::BLACK,
        "white" => Color::WHITE,
        "transparent" => Color::TRANSPARENT,
        "red" => Color::rgb(255, 0, 0),
        "green" => Color::rgb(0, 128, 0),
        "blue" => Color::rgb(0, 0, 255),
        "yellow" => Color::rgb(255, 255, 0),
        "gray" | "grey" => Color::rgb(128, 128, 128),
        "lightgray" | "lightgrey" => Color::rgb(211, 211, 211),
        "darkgray" | "darkgrey" => Color::rgb(169, 169, 169),
        "silver" => Color::rgb(192, 192, 192),
        "maroon" => Color::rgb(128, 0, 0),
        "purple" => Color::rgb(128, 0, 128),
        "fuchsia" | "magenta" => Color::rgb(255, 0, 255),
        "lime" => Color::rgb(0, 255, 0),
        "olive" => Color::rgb(128, 128, 0),
        "navy" => Color::rgb(0, 0, 128),
        "teal" => Color::rgb(0, 128, 128),
        "aqua" | "cyan" => Color::rgb(0, 255, 255),
        "orange" => Color::rgb(255, 165, 0),
        "pink" => Color::rgb(255, 192, 203),
        "brown" => Color::rgb(165, 42, 42),
        _ => return None,
    };
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_rule() {
        let s = parse("p { color: red; font-size: 14px; }");
        assert_eq!(s.rules.len(), 1);
        let r = &s.rules[0];
        assert_eq!(r.selectors.len(), 1);
        assert_eq!(r.selectors[0].compounds[0].tag.as_deref(), Some("p"));
        assert_eq!(r.declarations.len(), 2);
        assert_eq!(r.declarations[0].property, "color");
        assert!(matches!(r.declarations[0].value, Value::Color(_)));
    }

    #[test]
    fn selector_list_and_combinators() {
        let s = parse("div p, ul > li, a + span { color: black; }");
        assert_eq!(s.rules.len(), 1);
        let r = &s.rules[0];
        assert_eq!(r.selectors.len(), 3);
        assert_eq!(r.selectors[1].combinators, vec![Combinator::Child]);
        assert_eq!(r.selectors[2].combinators, vec![Combinator::AdjacentSibling]);
    }

    #[test]
    fn class_and_id() {
        let s = parse(".hero #title.huge { font-weight: 700; }");
        let sel = &s.rules[0].selectors[0];
        assert_eq!(sel.compounds[0].classes, vec!["hero".to_string()]);
        assert_eq!(sel.compounds[1].id.as_deref(), Some("title"));
        assert_eq!(sel.compounds[1].classes, vec!["huge".to_string()]);
    }

    #[test]
    fn hex_colors() {
        let s = parse("a { color: #fff; background: #00aaff; }");
        match &s.rules[0].declarations[0].value {
            Value::Color(c) => assert_eq!(*c, Color::WHITE),
            _ => panic!(),
        }
        match &s.rules[0].declarations[1].value {
            Value::Color(c) => assert_eq!(*c, Color::rgb(0, 0xaa, 0xff)),
            _ => panic!(),
        }
    }

    #[test]
    fn rgb_function() {
        let s = parse("a { color: rgb(10, 20, 30); }");
        match &s.rules[0].declarations[0].value {
            Value::Color(c) => assert_eq!(*c, Color::rgb(10, 20, 30)),
            _ => panic!(),
        }
    }

    #[test]
    fn lengths_and_keywords() {
        let s = parse("div { width: 50%; height: 10em; display: block; }");
        assert!(matches!(&s.rules[0].declarations[0].value, Value::Percentage(_)));
        assert!(matches!(&s.rules[0].declarations[1].value, Value::Length(_, Unit::Em)));
        match &s.rules[0].declarations[2].value {
            Value::Keyword(k) => assert_eq!(k, "block"),
            _ => panic!(),
        }
    }

    #[test]
    fn skips_at_rules() {
        let s = parse("@media print { p { color: red; } } body { color: blue; }");
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].selectors[0].compounds[0].tag.as_deref(), Some("body"));
    }

    #[test]
    fn ignores_pseudo_classes() {
        let s = parse("a:hover { color: red; }");
        let sel = &s.rules[0].selectors[0];
        assert_eq!(sel.compounds[0].tag.as_deref(), Some("a"));
    }

    #[test]
    fn comments_are_ignored() {
        let s = parse("/* hi */ p /* inline */ { color: /* x */ red; }");
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].declarations.len(), 1);
    }

    #[test]
    fn margin_shorthand_as_list() {
        let s = parse("p { margin: 1em 0 2px; }");
        match &s.rules[0].declarations[0].value {
            Value::List(v) => assert_eq!(v.len(), 3),
            other => panic!("expected list, got {other:?}"),
        }
    }
}
