//! CSS parser. Recursive-descent over the raw character stream — no separate
//! token type. Skips `@`-rules entirely. Ignores `!important`.
//!
//! Stores (rather than discards) pseudo-classes, pseudo-elements, and
//! attribute selectors so the cascade can decide whether to honor them
//! (most pseudo-classes match no element today and will be wired up in
//! phase 6 once we have interaction state).

use super::types::{
    AttributeOp, AttributeSelector, CalcExpr, Color, Combinator, Declaration, Rule, Selector,
    SimpleSelector, Stylesheet, Unit, Value,
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
        let mut pseudo_element = None;

        let (first, pe) = self.parse_compound()?;
        compounds.push(first);
        if pe.is_some() {
            pseudo_element = pe;
        }

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
            if let Some((next, pe)) = self.parse_compound() {
                combinators.push(combinator);
                compounds.push(next);
                if pe.is_some() {
                    pseudo_element = pe;
                }
            } else {
                break;
            }
        }

        Some(Selector {
            compounds,
            combinators,
            pseudo_element,
        })
    }

    /// Returns the compound selector and an optional pseudo-element name
    /// (e.g. "before") that was found on this compound.
    fn parse_compound(&mut self) -> Option<(SimpleSelector, Option<String>)> {
        let mut ss = SimpleSelector::default();
        let mut pseudo_element = None;
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
                    let is_double = self.peek() == Some(':');
                    if is_double {
                        self.advance();
                    }
                    let name = self.parse_ident()?.to_ascii_lowercase();
                    // Some pseudo-classes take an argument: :not(...), :nth-child(...).
                    // We parse-and-discard the argument so the rest of the selector
                    // doesn't get confused.
                    if self.peek() == Some('(') {
                        self.skip_balanced('(', ')');
                    }
                    if is_double || is_known_pseudo_element(&name) {
                        pseudo_element = Some(name);
                    } else {
                        ss.pseudo_classes.push(name);
                    }
                    had = true;
                }
                Some('[') => {
                    if let Some(attr) = self.parse_attribute_selector() {
                        ss.attributes.push(attr);
                    } else {
                        // recovery
                        self.skip_balanced('[', ']');
                    }
                    had = true;
                }
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                    let tag = self.parse_ident()?.to_ascii_lowercase();
                    ss.tag = Some(tag);
                    had = true;
                }
                _ => break,
            }
        }
        if had {
            Some((ss, pseudo_element))
        } else {
            None
        }
    }

    fn parse_attribute_selector(&mut self) -> Option<AttributeSelector> {
        if !self.consume('[') {
            return None;
        }
        self.skip_ws();
        let name = self.parse_ident()?.to_ascii_lowercase();
        self.skip_ws();
        // Operator
        let op = match self.peek() {
            Some(']') => {
                self.advance();
                return Some(AttributeSelector {
                    name,
                    op: AttributeOp::Exists,
                    value: None,
                });
            }
            Some('=') => {
                self.advance();
                AttributeOp::Equals
            }
            Some('~') => {
                self.advance();
                self.consume('=');
                AttributeOp::Includes
            }
            Some('|') => {
                self.advance();
                self.consume('=');
                AttributeOp::DashPrefix
            }
            Some('^') => {
                self.advance();
                self.consume('=');
                AttributeOp::Starts
            }
            Some('$') => {
                self.advance();
                self.consume('=');
                AttributeOp::Ends
            }
            Some('*') => {
                self.advance();
                self.consume('=');
                AttributeOp::Contains
            }
            _ => return None,
        };
        self.skip_ws();
        let value = match self.peek() {
            Some('"') | Some('\'') => self.parse_string_lit(),
            _ => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_whitespace() || c == ']' {
                        break;
                    }
                    self.advance();
                }
                Some(self.input[start..self.pos].to_string())
            }
        };
        self.skip_ws();
        // Optional case-insensitivity flag: i or s — we ignore it.
        if matches!(self.peek(), Some('i') | Some('s') | Some('I') | Some('S')) {
            self.advance();
        }
        self.skip_ws();
        self.consume(']');
        Some(AttributeSelector { name, op, value })
    }

    fn parse_string_lit(&mut self) -> Option<String> {
        let quote = self.advance()?;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == quote {
                self.advance();
                return Some(s);
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
        Some(s)
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
        // Custom properties: a name that starts with `--`.
        let property = if self.input[self.pos..].starts_with("--") {
            self.pos += 2;
            let name = self.parse_ident()?;
            format!("--{name}")
        } else {
            self.parse_ident()?.to_ascii_lowercase()
        };
        self.skip_ws();
        if !self.consume(':') {
            return None;
        }
        let value = self.parse_value_list();
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
            '"' | '\'' => self.parse_string_lit().map(Value::String),
            '#' => self.parse_hex_color(),
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.parse_numeric(),
            c if c.is_ascii_alphabetic() || c == '_' => self.parse_ident_or_function(),
            _ => None,
        }
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
        let n = self.parse_number()?;
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
            _ => return Some(Value::Number(n)),
        };
        Some(Value::Length(n, unit))
    }

    fn parse_number(&mut self) -> Option<f32> {
        let start = self.pos;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.advance();
        }
        let mut had_digits = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
                had_digits = true;
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                    had_digits = true;
                } else {
                    break;
                }
            }
        }
        if !had_digits {
            self.pos = start;
            return None;
        }
        self.input[start..self.pos].parse().ok()
    }

    fn parse_ident_or_function(&mut self) -> Option<Value> {
        let name = self.parse_ident()?;
        if self.peek() == Some('(') {
            self.advance();
            let lname = name.to_ascii_lowercase();
            let value = match lname.as_str() {
                "var" => self.parse_var_call(),
                "calc" => self.parse_calc_call(),
                "url" => self.parse_url_call(),
                "rgb" | "rgba" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    rgb_from_args(&args)
                }
                _ => {
                    // Unknown function — eat and represent as keyword.
                    self.skip_balanced_after_open('(', ')');
                    Some(Value::Keyword(lname))
                }
            };
            return value;
        }
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

    fn parse_var_call(&mut self) -> Option<Value> {
        self.skip_ws();
        if !self.input[self.pos..].starts_with("--") {
            self.skip_balanced_after_open('(', ')');
            return Some(Value::Keyword(String::new()));
        }
        self.pos += 2;
        let name_body = self.parse_ident()?;
        let name = format!("--{name_body}");
        self.skip_ws();
        let fallback = if self.peek() == Some(',') {
            self.advance();
            self.skip_ws();
            self.parse_one_value().map(Box::new)
        } else {
            None
        };
        self.skip_ws();
        self.consume(')');
        Some(Value::Var { name, fallback })
    }

    fn parse_calc_call(&mut self) -> Option<Value> {
        let expr = self.parse_calc_expression()?;
        self.skip_ws();
        self.consume(')');
        Some(Value::Calc(Box::new(expr)))
    }

    /// Recursive-descent for calc expressions: handles +, -, *, / with the
    /// usual precedence and parens.
    fn parse_calc_expression(&mut self) -> Option<CalcExpr> {
        let mut left = self.parse_calc_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.advance();
                    self.skip_ws();
                    let right = self.parse_calc_term()?;
                    left = CalcExpr::Add(Box::new(left), Box::new(right));
                }
                Some('-') => {
                    self.advance();
                    self.skip_ws();
                    let right = self.parse_calc_term()?;
                    left = CalcExpr::Sub(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Some(left)
    }

    fn parse_calc_term(&mut self) -> Option<CalcExpr> {
        let mut left = self.parse_calc_factor()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.advance();
                    self.skip_ws();
                    let right = self.parse_calc_factor()?;
                    left = CalcExpr::Mul(Box::new(left), Box::new(right));
                }
                Some('/') => {
                    self.advance();
                    self.skip_ws();
                    let right = self.parse_calc_factor()?;
                    left = CalcExpr::Div(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Some(left)
    }

    fn parse_calc_factor(&mut self) -> Option<CalcExpr> {
        self.skip_ws();
        if self.peek() == Some('(') {
            self.advance();
            let inner = self.parse_calc_expression()?;
            self.skip_ws();
            self.consume(')');
            return Some(inner);
        }
        if self.input[self.pos..].starts_with("var(") {
            self.pos += 4;
            let var_val = self.parse_var_call()?;
            return match var_val {
                Value::Var { name, fallback } => Some(CalcExpr::Var(name, fallback)),
                _ => None,
            };
        }
        let val = self.parse_one_value()?;
        match val {
            Value::Length(n, u) => Some(CalcExpr::Length(n, u)),
            Value::Percentage(p) => Some(CalcExpr::Percentage(p)),
            Value::Number(n) => Some(CalcExpr::Number(n)),
            _ => None,
        }
    }

    fn parse_url_call(&mut self) -> Option<Value> {
        self.skip_ws();
        let mut url = String::new();
        match self.peek() {
            Some('"') | Some('\'') => {
                url = self.parse_string_lit().unwrap_or_default();
            }
            _ => {
                while let Some(c) = self.peek() {
                    if c == ')' || c.is_ascii_whitespace() {
                        break;
                    }
                    url.push(c);
                    self.advance();
                }
            }
        }
        self.skip_ws();
        self.consume(')');
        Some(Value::Url(url))
    }

    fn parse_ident(&mut self) -> Option<String> {
        let start = self.pos;
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
        self.skip_balanced_after_open(open, close);
    }

    /// Like `skip_balanced` but assumes we've already consumed the opening
    /// character; useful when peek-and-advance was done elsewhere.
    fn skip_balanced_after_open(&mut self, open: char, close: char) {
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
            while let Some(c) = self.peek() {
                if c.is_ascii_whitespace() {
                    self.advance();
                } else {
                    break;
                }
            }
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

fn is_known_pseudo_element(name: &str) -> bool {
    matches!(
        name,
        "before" | "after" | "first-line" | "first-letter" | "placeholder" | "marker"
    )
}

fn rgb_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let r = number_to_byte(&args[0]);
    let g = number_to_byte(&args[1]);
    let b = number_to_byte(&args[2]);
    let a = if args.len() >= 4 {
        match &args[3] {
            Value::Number(n) => (n * 255.0).clamp(0.0, 255.0) as u8,
            Value::Percentage(p) => (p * 2.55).clamp(0.0, 255.0) as u8,
            _ => 255,
        }
    } else {
        255
    };
    Some(Value::Color(Color { r, g, b, a }))
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
        assert_eq!(s.rules[0].selectors[0].compounds[0].tag.as_deref(), Some("p"));
        assert_eq!(s.rules[0].declarations.len(), 2);
    }

    #[test]
    fn attribute_selector_exists() {
        let s = parse("input[disabled] { color: gray; }");
        let attrs = &s.rules[0].selectors[0].compounds[0].attributes;
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "disabled");
        assert_eq!(attrs[0].op, AttributeOp::Exists);
    }

    #[test]
    fn attribute_selector_value_ops() {
        let s = parse(r#"a[href^="https"] { color: green; } [type=text] {} [class~="foo"] {}"#);
        assert_eq!(
            s.rules[0].selectors[0].compounds[0].attributes[0].op,
            AttributeOp::Starts
        );
        assert_eq!(
            s.rules[1].selectors[0].compounds[0].attributes[0].op,
            AttributeOp::Equals
        );
        assert_eq!(
            s.rules[2].selectors[0].compounds[0].attributes[0].op,
            AttributeOp::Includes
        );
    }

    #[test]
    fn pseudo_class_stored_not_discarded() {
        let s = parse("a:hover { color: red; }");
        let c = &s.rules[0].selectors[0].compounds[0];
        assert_eq!(c.pseudo_classes, vec!["hover".to_string()]);
    }

    #[test]
    fn pseudo_element_stored() {
        let s = parse("p::before { content: ''; }");
        assert_eq!(
            s.rules[0].selectors[0].pseudo_element.as_deref(),
            Some("before")
        );
    }

    #[test]
    fn var_value_parsed() {
        let s = parse(".x { color: var(--main, blue); }");
        match &s.rules[0].declarations[0].value {
            Value::Var { name, fallback } => {
                assert_eq!(name, "--main");
                assert!(matches!(fallback.as_deref(), Some(Value::Color(_))));
            }
            other => panic!("expected Var, got {other:?}"),
        }
    }

    #[test]
    fn custom_property_declaration() {
        let s = parse(":root { --primary: #336699; }");
        let d = &s.rules[0].declarations[0];
        assert_eq!(d.property, "--primary");
        assert!(matches!(d.value, Value::Color(_)));
    }

    #[test]
    fn calc_simple() {
        let s = parse("p { width: calc(100% - 20px); }");
        match &s.rules[0].declarations[0].value {
            Value::Calc(expr) => match expr.as_ref() {
                CalcExpr::Sub(_, _) => {}
                other => panic!("expected Sub, got {other:?}"),
            },
            other => panic!("expected Calc, got {other:?}"),
        }
    }

    #[test]
    fn calc_with_precedence() {
        let s = parse("p { width: calc(10px + 4 * 2px); }");
        match &s.rules[0].declarations[0].value {
            Value::Calc(expr) => match expr.as_ref() {
                CalcExpr::Add(_, mul) => match mul.as_ref() {
                    CalcExpr::Mul(_, _) => {}
                    other => panic!("expected Mul on right, got {other:?}"),
                },
                other => panic!("expected Add, got {other:?}"),
            },
            other => panic!("expected Calc, got {other:?}"),
        }
    }

    #[test]
    fn url_function() {
        let s = parse(r#"body { background: url("bg.png"); }"#);
        match &s.rules[0].declarations[0].value {
            Value::Url(u) => assert_eq!(u, "bg.png"),
            other => panic!("expected Url, got {other:?}"),
        }
    }

    #[test]
    fn margin_shorthand_list() {
        let s = parse("p { margin: 1em 0 2px; }");
        match &s.rules[0].declarations[0].value {
            Value::List(v) => assert_eq!(v.len(), 3),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn skips_at_rules() {
        let s = parse("@media print { p { color: red; } } body { color: blue; }");
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].selectors[0].compounds[0].tag.as_deref(), Some("body"));
    }

    #[test]
    fn comments_are_ignored() {
        let s = parse("/* hi */ p /* x */ { color: /* y */ red; }");
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].declarations.len(), 1);
    }
}
