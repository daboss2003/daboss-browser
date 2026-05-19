//! CSS parser. Recursive-descent over the raw character stream — no separate
//! token type. Skips `@`-rules entirely. Ignores `!important`.
//!
//! Stores (rather than discards) pseudo-classes, pseudo-elements, and
//! attribute selectors so the cascade can decide whether to honor them
//! (most pseudo-classes match no element today and will be wired up in
//! phase 6 once we have interaction state).

use super::types::{
    AttributeOp, AttributeSelector, CalcExpr, Color, Combinator, Declaration, FontFace,
    FontSource, FontStyle, KeyframeStep, KeyframesAnim, MediaBlock, MediaCondition, MediaQuery,
    Nth, PseudoClass, Rule, Selector, SimpleSelector, Stylesheet, Unit, Value,
};

pub fn parse(input: &str) -> Stylesheet {
    Parser::new(input).parse_stylesheet()
}

pub fn parse_inline_declarations(input: &str) -> Vec<Declaration> {
    let mut p = Parser::new(input);
    p.parse_declarations()
}

/// Parse a standalone selector list (e.g. `"div.foo, p#bar"`) — used by the
/// JS subsystem to back `document.querySelector` / `querySelectorAll`.
/// Returns `None` if the input doesn't contain a single valid selector.
pub fn parse_selector_list_str(input: &str) -> Option<Vec<Selector>> {
    let mut p = Parser::new(input);
    let sels = p.parse_selector_list()?;
    // Ignore trailing junk — the caller passes a selector string, not a rule.
    Some(sels)
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
        let mut sheet = Stylesheet::default();
        loop {
            self.skip_ws();
            if self.eof() {
                break;
            }
            if self.peek() == Some('@') {
                self.parse_at_rule(&mut sheet);
                continue;
            }
            if self.peek() == Some('}') {
                self.advance();
                continue;
            }
            if let Some(rule) = self.parse_rule() {
                sheet.rules.push(rule);
            } else {
                self.skip_to_block_end_or_semi();
            }
        }
        sheet
    }

    /// Handle a single top-level `@`-rule. Recognised:
    ///  * `@media` — its body is recursively parsed as a normal
    ///    stylesheet (nested `@media`s collapse, which is fine for our
    ///    purposes).
    ///  * `@font-face` — collected for later web-font fetching.
    ///  * `@keyframes` / `@-webkit-keyframes` — stored verbatim.
    ///
    /// Everything else (`@supports`, `@page`, `@import`, etc.) is
    /// skipped through `skip_at_rule` so we don't choke on it.
    fn parse_at_rule(&mut self, sheet: &mut Stylesheet) {
        // Snapshot in case we decide to skip.
        let _start_pos = self.pos;
        self.advance(); // '@'
        let name = self.parse_ident().unwrap_or_default().to_ascii_lowercase();
        match name.as_str() {
            "media" => {
                let query_text = self.read_prelude_until_brace();
                if !self.consume('{') {
                    return;
                }
                let inner = self.parse_at_block_inner();
                let query = parse_media_query(&query_text);
                sheet.media_blocks.push(MediaBlock {
                    query,
                    rules: inner.rules,
                });
                // Pull any nested @font-face / @keyframes out so they
                // aren't trapped inside an @media we'll ignore.
                sheet.font_faces.extend(inner.font_faces);
                sheet.keyframes.extend(inner.keyframes);
            }
            "container" => {
                // Container queries. Real evaluation needs layout-time
                // access to the matched container's box size; for now
                // we evaluate the condition against the viewport
                // (mirrors `@media (...)` semantics). Wrong when the
                // container differs from the viewport, but at least
                // the rules are picked up by cascade rather than
                // dropped on the floor. Strip the optional container
                // name preceding the condition list.
                let prelude = self.read_prelude_until_brace();
                if !self.consume('{') {
                    return;
                }
                let inner = self.parse_at_block_inner();
                let condition_text = strip_container_name(&prelude);
                let query = parse_media_query(&condition_text);
                sheet.media_blocks.push(MediaBlock {
                    query,
                    rules: inner.rules,
                });
                sheet.font_faces.extend(inner.font_faces);
                sheet.keyframes.extend(inner.keyframes);
            }
            "font-face" => {
                self.skip_ws();
                if !self.consume('{') {
                    return;
                }
                let decls = self.parse_declarations();
                self.skip_ws();
                self.consume('}');
                if let Some(ff) = font_face_from_declarations(&decls) {
                    sheet.font_faces.push(ff);
                }
            }
            "keyframes" | "-webkit-keyframes" | "-moz-keyframes" => {
                let anim_name = self.read_prelude_until_brace().trim().to_string();
                if !self.consume('{') {
                    return;
                }
                let steps = self.parse_keyframes_body();
                sheet.keyframes.push(KeyframesAnim {
                    name: anim_name,
                    steps,
                });
            }
            _ => {
                // Unknown @-rule. Skip its prelude + optional block.
                self.skip_remaining_at_rule();
            }
        }
    }

    fn read_prelude_until_brace(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c == '{' || c == ';' {
                break;
            }
            out.push(c);
            self.advance();
        }
        out
    }

    /// Parse the inside of an `@media` body as if it were a top-level
    /// stylesheet, handling nested at-rules / regular rules until the
    /// closing `}`. Returns the accumulated stylesheet for splicing into
    /// the outer one.
    fn parse_at_block_inner(&mut self) -> Stylesheet {
        let mut sheet = Stylesheet::default();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return sheet,
                Some('}') => {
                    self.advance();
                    return sheet;
                }
                Some('@') => {
                    self.parse_at_rule(&mut sheet);
                }
                _ => {
                    if let Some(rule) = self.parse_rule() {
                        sheet.rules.push(rule);
                    } else {
                        self.skip_to_block_end_or_semi();
                    }
                }
            }
        }
    }

    fn parse_keyframes_body(&mut self) -> Vec<KeyframeStep> {
        let mut steps = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return steps,
                Some('}') => {
                    self.advance();
                    return steps;
                }
                _ => {
                    let label = self.read_prelude_until_brace();
                    if !self.consume('{') {
                        // Malformed — bail.
                        return steps;
                    }
                    let decls = self.parse_declarations();
                    self.skip_ws();
                    self.consume('}');
                    for off in keyframe_offsets(&label) {
                        steps.push(KeyframeStep {
                            offset: off,
                            declarations: decls.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Read the contents of a `(...)` group at the current position,
    /// consuming the opening and closing parens. Nested parens are
    /// tracked. Returns the inner text (no enclosing parens).
    fn read_balanced_paren_contents(&mut self) -> String {
        if self.peek() != Some('(') {
            return String::new();
        }
        self.advance(); // '('
        let mut depth = 1;
        let start = self.pos;
        while let Some(c) = self.peek() {
            match c {
                '(' => {
                    depth += 1;
                    self.advance();
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let out = self.input[start..self.pos].to_string();
                        self.advance(); // ')'
                        return out;
                    }
                    self.advance();
                }
                _ => {
                    self.advance();
                }
            }
        }
        self.input[start..self.pos].to_string()
    }

    fn skip_remaining_at_rule(&mut self) {
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

                    // Functional pseudo-classes carry an argument list.
                    let arg = if self.peek() == Some('(') {
                        Some(self.read_balanced_paren_contents())
                    } else {
                        None
                    };

                    if is_double || is_known_pseudo_element(&name) {
                        pseudo_element = Some(name);
                    } else if let Some(arg) = arg {
                        let arg = arg.trim();
                        let pc = match name.as_str() {
                            "not" => PseudoClass::Not(parse_inner_selector_list(arg)),
                            "is" | "matches" => {
                                PseudoClass::Is(parse_inner_selector_list(arg))
                            }
                            "where" => PseudoClass::Where(parse_inner_selector_list(arg)),
                            "has" => PseudoClass::Has(parse_inner_selector_list(arg)),
                            "nth-child" => PseudoClass::NthChild(parse_nth_arg(arg)),
                            "nth-of-type" => PseudoClass::NthOfType(parse_nth_arg(arg)),
                            "nth-last-child" => {
                                PseudoClass::NthLastChild(parse_nth_arg(arg))
                            }
                            "nth-last-of-type" => {
                                PseudoClass::NthLastOfType(parse_nth_arg(arg))
                            }
                            // Unknown functional pseudo — keep the name so
                            // specificity counts but the cascade rejects it.
                            _ => PseudoClass::Name(name),
                        };
                        ss.pseudo_classes.push(pc);
                    } else {
                        ss.pseudo_classes.push(PseudoClass::Name(name));
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
        // A dashed-ident (`--foo`) must be tried before the numeric
        // branch — otherwise `-` falls into `parse_numeric` which
        // resets and returns None, leaving the lexer to consume the
        // ident one character at a time and produce nothing.
        if c == '-' && self.input[self.pos..].starts_with("--") {
            let start = self.pos;
            self.advance(); // '-'
            self.advance(); // '-'
            while let Some(nc) = self.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' || nc == '-' {
                    self.advance();
                } else {
                    break;
                }
            }
            let name = self.input[start..self.pos].to_string();
            // Preserve case for dashed-idents (--Foo and --foo differ).
            return Some(Value::Keyword(name));
        }
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
            "fr" => Unit::Fr,
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
                "linear-gradient" => self.parse_linear_gradient(),
                "rgb" | "rgba" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    rgb_from_args(&args)
                }
                "hsl" | "hsla" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    hsl_from_args(&args)
                }
                "oklch" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    oklch_from_args(&args)
                }
                "oklab" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    oklab_from_args(&args)
                }
                "lab" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    lab_from_args(&args)
                }
                "lch" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    lch_from_args(&args)
                }
                "color" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    color_func_from_args(&args)
                }
                "color-mix" => {
                    let args = self.parse_function_args();
                    self.consume(')');
                    color_mix_from_args(&args)
                }
                _ => {
                    // Unknown function — keep args so e.g. `transform:
                    // translate(10px, 20px)` can be picked up later.
                    let args = self.parse_function_args();
                    self.consume(')');
                    Some(Value::Function { name: lname, args })
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

    /// Parse `linear-gradient(angle, color, color, ...)`. Accepts:
    ///   - leading angle like `45deg` (stored as a `Value::Number(45)` by
    ///     `parse_numeric` because `deg` isn't in our `Unit` enum),
    ///   - keyword direction like `to bottom` / `to top` etc.,
    ///   - or no leading direction at all (defaults to 180° = top → bottom).
    /// Colors are spread evenly across stop positions.
    fn parse_linear_gradient(&mut self) -> Option<Value> {
        let args = self.parse_function_args();
        self.consume(')');
        let mut angle_deg = 180.0_f32;
        let mut saw_to = false;
        let mut colors: Vec<Color> = Vec::new();
        for arg in args {
            match arg {
                Value::Number(n) => angle_deg = n,
                Value::Color(c) => colors.push(c),
                Value::Keyword(k) => match k.as_str() {
                    "to" => {
                        saw_to = true;
                    }
                    "bottom" if saw_to => angle_deg = 180.0,
                    "top" if saw_to => angle_deg = 0.0,
                    "right" if saw_to => angle_deg = 90.0,
                    "left" if saw_to => angle_deg = 270.0,
                    _ => {}
                },
                _ => {}
            }
        }
        if colors.is_empty() {
            return Some(Value::Keyword("linear-gradient".into()));
        }
        let denom = (colors.len() - 1).max(1) as f32;
        let stops = colors
            .iter()
            .enumerate()
            .map(|(i, c)| (i as f32 / denom, *c))
            .collect();
        Some(Value::LinearGradient { angle_deg, stops })
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
                // Jump straight to the comment terminator — incrementing by
                // one byte at a time can land us inside a multi-byte UTF-8
                // character (em-dash, smart quotes, etc.) and panic on the
                // next slice. `find` searches by bytes safely.
                match self.input[self.pos..].find("*/") {
                    Some(end) => self.pos += end + 2,
                    None => self.pos = self.input.len(),
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

fn parse_inner_selector_list(arg: &str) -> Vec<Selector> {
    Parser::new(arg).parse_selector_list().unwrap_or_default()
}

/// Parse an `:nth-*()` argument. Accepts `odd`, `even`, a bare integer,
/// or the full `An+B` form (with optional sign on A, and optional B).
/// Anything else falls back to `0n+0`, which matches no element.
fn parse_nth_arg(arg: &str) -> Nth {
    let s = arg.trim().to_ascii_lowercase();
    if s == "odd" {
        return Nth::ODD;
    }
    if s == "even" {
        return Nth::EVEN;
    }
    // Strip whitespace inside the expression: `2n + 1` → `2n+1`.
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();

    if let Some(idx) = s.find('n') {
        let (a_part, rest) = s.split_at(idx);
        let a: i32 = match a_part {
            "" | "+" => 1,
            "-" => -1,
            other => other.parse().unwrap_or(0),
        };
        let b_part = &rest[1..]; // skip 'n'
        let b: i32 = if b_part.is_empty() {
            0
        } else {
            b_part.parse().unwrap_or(0)
        };
        return Nth { a, b };
    }
    Nth {
        a: 0,
        b: s.parse().unwrap_or(0),
    }
}

/// Parse an `@media` query prelude (everything between `@media` and `{`).
/// Comma separates alternatives; `and` joins conditions. We support
/// media types (`screen`/`print`/`all`) and the small set of named
/// features the rest of the engine actually reads.
fn parse_media_query(input: &str) -> MediaQuery {
    let mut query = MediaQuery::default();
    for alt in input.split(',') {
        let conds = parse_media_alternative(alt);
        if !conds.is_empty() {
            query.alternatives.push(conds);
        }
    }
    query
}

fn parse_media_alternative(input: &str) -> Vec<MediaCondition> {
    // Indices into `chars` must be indices into the same slice we then
    // re-slice; trimming `input` returns a substring whose offsets are
    // not the offsets of `input`, so shadow it before tokenising.
    let s = input.trim();
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_whitespace() {
            chars.next();
            continue;
        }
        if c == '(' {
            // Find the matching ')'. Both `j` (offset within `s[i..]`)
            // and `end` (offset within `s`) are byte indices.
            let mut depth = 0;
            let mut end = s.len();
            for (j, ch) in s[i..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + j;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let body = &s[i + 1..end];
            out.push(parse_media_feature(body));
            // Skip the iterator past the closing paren itself.
            while let Some(&(j, _)) = chars.peek() {
                if j > end {
                    break;
                }
                chars.next();
            }
            continue;
        }
        // Bare token.
        let start = i;
        while let Some(&(_, ch)) = chars.peek() {
            if ch.is_ascii_whitespace() || ch == '(' {
                break;
            }
            chars.next();
        }
        let end = chars.peek().map(|(j, _)| *j).unwrap_or(s.len());
        let tok = s[start..end].to_ascii_lowercase();
        match tok.as_str() {
            "and" | "only" | "" => {}
            "not" => {
                out.push(MediaCondition::Unsupported("not".into()));
            }
            "screen" | "print" | "all" | "speech" => {
                out.push(MediaCondition::MediaType(tok));
            }
            other => {
                out.push(MediaCondition::Unsupported(other.into()));
            }
        }
    }
    out
}

fn parse_media_feature(body: &str) -> MediaCondition {
    let body = body.trim();
    let (name, value) = match body.split_once(':') {
        Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim()),
        None => (body.to_ascii_lowercase(), ""),
    };
    let px = parse_css_length_px(value);
    match name.as_str() {
        "min-width" => px.map(MediaCondition::MinWidth)
            .unwrap_or_else(|| MediaCondition::Unsupported(body.to_string())),
        "max-width" => px.map(MediaCondition::MaxWidth)
            .unwrap_or_else(|| MediaCondition::Unsupported(body.to_string())),
        "min-height" => px.map(MediaCondition::MinHeight)
            .unwrap_or_else(|| MediaCondition::Unsupported(body.to_string())),
        "max-height" => px.map(MediaCondition::MaxHeight)
            .unwrap_or_else(|| MediaCondition::Unsupported(body.to_string())),
        "width" => px.map(MediaCondition::ExactWidth)
            .unwrap_or_else(|| MediaCondition::Unsupported(body.to_string())),
        "orientation" => MediaCondition::Orientation(value.to_ascii_lowercase()),
        "prefers-color-scheme" => MediaCondition::PrefersColorScheme(value.to_ascii_lowercase()),
        _ => MediaCondition::Unsupported(body.to_string()),
    }
}

/// Parse a CSS length like `"360px"`, `"45em"`, etc. into pixels.
/// `em`/`rem` use 16px as the assumed root size, matching the cascade's
/// default. Unit-less zero is accepted.
fn parse_css_length_px(s: &str) -> Option<f32> {
    let s = s.trim();
    if s == "0" {
        return Some(0.0);
    }
    let (num, unit) = split_number_unit(s)?;
    Some(match unit {
        "" | "px" => num,
        "em" | "rem" => num * 16.0,
        "pt" => num * 1.333,
        "pc" => num * 16.0,
        "in" => num * 96.0,
        "cm" => num * 37.795,
        "mm" => num * 3.7795,
        _ => return None,
    })
}

fn split_number_unit(s: &str) -> Option<(f32, &str)> {
    let mut split = s.len();
    for (i, c) in s.char_indices() {
        if !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+') {
            split = i;
            break;
        }
    }
    let (num, unit) = s.split_at(split);
    Some((num.parse::<f32>().ok()?, unit))
}

fn font_face_from_declarations(decls: &[Declaration]) -> Option<FontFace> {
    let mut ff = FontFace::default();
    for d in decls {
        match d.property.as_str() {
            "font-family" => {
                if let Some(name) = first_string_or_keyword(&d.value) {
                    ff.family = name;
                }
            }
            "src" => {
                ff.sources = parse_font_face_src(&d.value);
            }
            "font-weight" => {
                if let Value::Number(n) = &d.value {
                    ff.weight = Some(*n as u16);
                }
            }
            "font-style" => {
                if let Value::Keyword(k) = &d.value {
                    if k.eq_ignore_ascii_case("italic") {
                        ff.style = Some(FontStyle::Italic);
                    } else if k.eq_ignore_ascii_case("normal") {
                        ff.style = Some(FontStyle::Normal);
                    }
                }
            }
            _ => {}
        }
    }
    if ff.family.is_empty() {
        None
    } else {
        Some(ff)
    }
}

fn first_string_or_keyword(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Keyword(k) => Some(k.clone()),
        Value::List(xs) => xs.iter().find_map(first_string_or_keyword),
        _ => None,
    }
}

fn parse_font_face_src(v: &Value) -> Vec<FontSource> {
    let mut out = Vec::new();
    let mut visit = |val: &Value| match val {
        Value::Url(u) => out.push(FontSource::Url(u.clone(), None)),
        Value::String(s) => out.push(FontSource::Local(s.clone())),
        _ => {}
    };
    match v {
        Value::List(xs) => {
            for x in xs {
                visit(x);
            }
        }
        other => visit(other),
    }
    out
}

/// Translate keyframe step labels (`from`, `to`, `50%`, `0%, 100%`) into
/// the normalised `0.0..=1.0` offsets the engine consumes.
fn keyframe_offsets(label: &str) -> Vec<f32> {
    let mut out = Vec::new();
    for piece in label.split(',') {
        let p = piece.trim().to_ascii_lowercase();
        match p.as_str() {
            "from" => out.push(0.0),
            "to" => out.push(1.0),
            _ => {
                let pct = p.trim_end_matches('%');
                if let Ok(n) = pct.parse::<f32>() {
                    out.push((n / 100.0).clamp(0.0, 1.0));
                }
            }
        }
    }
    out
}

fn is_known_pseudo_element(name: &str) -> bool {
    matches!(
        name,
        "before" | "after" | "first-line" | "first-letter" | "placeholder" | "marker"
    )
}

/// Strip an optional container-name token from an `@container`
/// prelude. The spec allows `@container <name>? <condition>`, so we
/// peel off the first ident only if a `(` follows it later.
fn strip_container_name(prelude: &str) -> String {
    let trimmed = prelude.trim();
    let first = trimmed.split_ascii_whitespace().next();
    if let Some(name) = first {
        if !name.starts_with('(')
            && trimmed
                .trim_start_matches(name)
                .trim_start()
                .starts_with('(')
        {
            return trimmed.trim_start_matches(name).trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Parse `container-type` / `container-name` for storage on
/// ComputedStyle so future layout-aware evaluation can consult them.
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

/// `Value` → angle in degrees. Bare numbers are degrees by default
/// (matches modern color syntax where `oklch(L C 200)` is allowed).
fn value_as_degrees(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n),
        // We don't carry deg/rad/turn unit info on Length values yet
        // so degrees is the assumed default.
        Value::Length(n, _) => Some(*n),
        _ => None,
    }
}

fn value_as_number(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Percentage(p) => Some(p / 100.0),
        _ => None,
    }
}

fn value_as_axis(v: &Value, scale: f32) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Percentage(p) => Some(p / 100.0 * scale),
        _ => None,
    }
}

fn alpha_from(args: &[Value], at: usize) -> u8 {
    match args.get(at) {
        Some(Value::Number(n)) => (n * 255.0).clamp(0.0, 255.0) as u8,
        Some(Value::Percentage(p)) => (p * 2.55).clamp(0.0, 255.0) as u8,
        _ => 255,
    }
}

fn srgb_to_byte_color(r: f32, g: f32, b: f32, a: u8) -> Color {
    Color {
        r: (r * 255.0).clamp(0.0, 255.0).round() as u8,
        g: (g * 255.0).clamp(0.0, 255.0).round() as u8,
        b: (b * 255.0).clamp(0.0, 255.0).round() as u8,
        a,
    }
}

// hsl(H S L [/ A])
fn hsl_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let h = value_as_degrees(&args[0])?;
    let s = value_as_number(&args[1])?;
    let l = value_as_number(&args[2])?;
    let a = alpha_from(args, 3);
    let (r, g, b) = hsl_to_srgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
    Some(Value::Color(srgb_to_byte_color(r, g, b, a)))
}

fn hsl_to_srgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = ((h % 360.0 + 360.0) % 360.0) / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r1 + m, g1 + m, b1 + m)
}

// oklch(L C H [/ A]) per CSS Color 4. L is in [0,1] (or 0%-100%).
// C is unbounded but typically <= 0.4. H is degrees.
fn oklch_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let l = value_as_number(&args[0])?;
    let c = match &args[1] {
        Value::Number(n) => *n,
        Value::Percentage(p) => p / 100.0 * 0.4,
        _ => return None,
    };
    let h = value_as_degrees(&args[2])?;
    let a = alpha_from(args, 3);
    let h_rad = h.to_radians();
    let oklab_a = c * h_rad.cos();
    let oklab_b = c * h_rad.sin();
    let (r, g, b) = oklab_to_srgb(l, oklab_a, oklab_b);
    Some(Value::Color(srgb_to_byte_color(r, g, b, a)))
}

fn oklab_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let l = value_as_number(&args[0])?;
    let a_ = value_as_axis(&args[1], 0.4)?;
    let b_ = value_as_axis(&args[2], 0.4)?;
    let alpha = alpha_from(args, 3);
    let (r, g, b) = oklab_to_srgb(l, a_, b_);
    Some(Value::Color(srgb_to_byte_color(r, g, b, alpha)))
}

/// OKLab → linear sRGB → sRGB. Matrices per Björn Ottosson's reference.
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;
    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;
    let lr = 4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3;
    let lg = -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3;
    let lb = -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3;
    (linear_to_srgb(lr), linear_to_srgb(lg), linear_to_srgb(lb))
}

fn linear_to_srgb(c: f32) -> f32 {
    let c = c.max(0.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    let c = c.max(0.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// lab(L a b [/ A]) — CIE Lab, D50 reference white.
fn lab_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let l = match &args[0] {
        Value::Number(n) => *n,
        Value::Percentage(p) => *p, // 0..100
        _ => return None,
    };
    let a_ = match &args[1] {
        Value::Number(n) => *n,
        Value::Percentage(p) => p / 100.0 * 125.0,
        _ => return None,
    };
    let b_ = match &args[2] {
        Value::Number(n) => *n,
        Value::Percentage(p) => p / 100.0 * 125.0,
        _ => return None,
    };
    let alpha = alpha_from(args, 3);
    let (r, g, b) = lab_to_srgb(l, a_, b_);
    Some(Value::Color(srgb_to_byte_color(r, g, b, alpha)))
}

fn lch_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 3 {
        return None;
    }
    let l = match &args[0] {
        Value::Number(n) => *n,
        Value::Percentage(p) => *p,
        _ => return None,
    };
    let c = match &args[1] {
        Value::Number(n) => *n,
        Value::Percentage(p) => p / 100.0 * 150.0,
        _ => return None,
    };
    let h = value_as_degrees(&args[2])?;
    let alpha = alpha_from(args, 3);
    let h_rad = h.to_radians();
    let a_ = c * h_rad.cos();
    let b_ = c * h_rad.sin();
    let (r, g, b) = lab_to_srgb(l, a_, b_);
    Some(Value::Color(srgb_to_byte_color(r, g, b, alpha)))
}

/// Lab (D50) → XYZ (D50) → sRGB. Uses the standard CIE conversion +
/// Bradford D50→D65 chromatic adaptation, then the sRGB matrix.
fn lab_to_srgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    // L is 0..100.
    let l = l / 100.0;
    let fy = (l + 0.16) / 1.16;
    let fx = a / 5.0 + fy;
    let fz = fy - b / 2.0;
    let kappa = 24389.0 / 27.0;
    let eps = 216.0 / 24389.0;
    let xr = if fx.powi(3) > eps { fx.powi(3) } else { (116.0 * fx - 16.0) / kappa };
    let yr = if l > kappa * eps / 116.0 { fy.powi(3) } else { l / kappa * 116.0 };
    let zr = if fz.powi(3) > eps { fz.powi(3) } else { (116.0 * fz - 16.0) / kappa };
    // D50 reference white.
    let (xd50, yd50, zd50) = (xr * 0.9642, yr * 1.0, zr * 0.8249);
    // Bradford D50 → D65.
    let xd65 = 0.9555766 * xd50 - 0.0230393 * yd50 + 0.0631636 * zd50;
    let yd65 = -0.0282895 * xd50 + 1.0099416 * yd50 + 0.0210077 * zd50;
    let zd65 = 0.0122982 * xd50 - 0.0204830 * yd50 + 1.3299098 * zd50;
    // XYZ (D65) → linear sRGB.
    let lr = 3.2404542 * xd65 - 1.5371385 * yd65 - 0.4985314 * zd65;
    let lg = -0.9692660 * xd65 + 1.8760108 * yd65 + 0.0415560 * zd65;
    let lb = 0.0556434 * xd65 - 0.2040259 * yd65 + 1.0572252 * zd65;
    (linear_to_srgb(lr), linear_to_srgb(lg), linear_to_srgb(lb))
}

/// `color-mix(in <space>, <color1> [<pct>], <color2> [<pct>])` — mix
/// two colours by mass. The `in <space>` prefix selects the
/// interpolation space; for the toy we always mix in linear sRGB,
/// which is close-enough for the common `oklch` / `srgb` spaces
/// people target. Percentages default to 50% / 50%.
fn color_mix_from_args(args: &[Value]) -> Option<Value> {
    // Expected structure (parser flattens commas): keyword "in",
    // keyword <space>, Color, [Percentage], Color, [Percentage].
    // We skip until we find two Color values.
    let mut colors: Vec<Color> = Vec::new();
    let mut weights: Vec<Option<f32>> = Vec::new();
    let mut last_was_color = false;
    for v in args {
        match v {
            Value::Color(c) => {
                colors.push(*c);
                weights.push(None);
                last_was_color = true;
            }
            Value::Percentage(p) if last_was_color => {
                if let Some(slot) = weights.last_mut() {
                    *slot = Some(p / 100.0);
                }
                last_was_color = false;
            }
            _ => {
                last_was_color = false;
            }
        }
    }
    if colors.len() < 2 {
        return None;
    }
    let (c1, c2) = (colors[0], colors[1]);
    let (w1, w2) = match (weights[0], weights[1]) {
        (None, None) => (0.5, 0.5),
        (Some(a), None) => (a, 1.0 - a),
        (None, Some(b)) => (1.0 - b, b),
        (Some(a), Some(b)) => {
            let total = (a + b).max(0.0001);
            (a / total, b / total)
        }
    };
    // Mix in linear sRGB by un-premultiplying then re-applying the
    // gamma curve. Tolerates the toy's sRGB-only Color storage.
    let lin = |c: u8| srgb_to_linear(c as f32 / 255.0);
    let unlin = |x: f32| (linear_to_srgb(x) * 255.0).round().clamp(0.0, 255.0) as u8;
    let mix = |a: u8, b: u8| unlin(lin(a) * w1 + lin(b) * w2);
    Some(Value::Color(Color {
        r: mix(c1.r, c2.r),
        g: mix(c1.g, c2.g),
        b: mix(c1.b, c2.b),
        a: ((c1.a as f32 * w1 + c2.a as f32 * w2).round() as u8).clamp(0, 255),
    }))
}

// color(space r g b [/ a]) — supports srgb, srgb-linear, display-p3.
fn color_func_from_args(args: &[Value]) -> Option<Value> {
    if args.len() < 4 {
        return None;
    }
    let space = match &args[0] {
        Value::Keyword(s) => s.to_ascii_lowercase(),
        _ => return None,
    };
    let r = value_as_number(&args[1])?;
    let g = value_as_number(&args[2])?;
    let b = value_as_number(&args[3])?;
    let a = alpha_from(args, 4);
    let (sr, sg, sb) = match space.as_str() {
        "srgb" => (r, g, b),
        "srgb-linear" => (linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(b)),
        "display-p3" => {
            // Display-P3 → linear-P3 → linear-sRGB via the standard
            // 3x3, then gamma-encode.
            let lr = srgb_to_linear(r);
            let lg = srgb_to_linear(g);
            let lb = srgb_to_linear(b);
            let sr_l = 1.2249401 * lr - 0.2249404 * lg + 0.0000004 * lb;
            let sg_l = -0.0420569 * lr + 1.0420571 * lg - 0.0000001 * lb;
            let sb_l = -0.0196376 * lr - 0.0786361 * lg + 1.0982735 * lb;
            (linear_to_srgb(sr_l), linear_to_srgb(sg_l), linear_to_srgb(sb_l))
        }
        _ => (r, g, b),
    };
    Some(Value::Color(srgb_to_byte_color(sr, sg, sb, a)))
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
    fn media_block_is_parsed_and_stored() {
        let s = parse(
            "@media (max-width: 600px) { p { color: red; } } body { color: blue; }",
        );
        assert_eq!(s.media_blocks.len(), 1);
        assert_eq!(s.media_blocks[0].rules.len(), 1);
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].selectors[0].compounds[0].tag.as_deref(), Some("body"));
        // The query alternative has one MinWidth/MaxWidth condition.
        assert!(matches!(
            s.media_blocks[0].query.alternatives[0][0],
            MediaCondition::MaxWidth(600.0)
        ));
    }

    #[test]
    fn font_face_collected() {
        let s = parse(r#"@font-face { font-family: "Foo"; src: url(foo.woff2); }"#);
        assert_eq!(s.font_faces.len(), 1);
        assert_eq!(s.font_faces[0].family, "Foo");
        assert!(matches!(
            s.font_faces[0].sources[0],
            FontSource::Url(ref u, _) if u == "foo.woff2"
        ));
    }

    #[test]
    fn keyframes_stored_with_offsets() {
        let s = parse(
            "@keyframes spin { from { opacity: 0; } 50% { opacity: 0.5; } to { opacity: 1; } }",
        );
        assert_eq!(s.keyframes.len(), 1);
        assert_eq!(s.keyframes[0].name, "spin");
        let offsets: Vec<f32> = s.keyframes[0]
            .steps
            .iter()
            .map(|st| st.offset)
            .collect();
        assert_eq!(offsets, vec![0.0, 0.5, 1.0]);
    }

    #[test]
    fn unknown_at_rule_is_skipped_without_breaking_sheet() {
        let s = parse("@charset \"utf-8\"; @supports (display: grid) { p { color: red; } } body { }");
        // No crash, `body` rule still parsed.
        assert!(s.rules.iter().any(|r| r
            .selectors
            .iter()
            .any(|s| s.compounds[0].tag.as_deref() == Some("body"))));
    }

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
        assert!(matches!(
            c.pseudo_classes[0],
            PseudoClass::Name(ref n) if n == "hover"
        ));
    }

    #[test]
    fn parses_not_functional_pseudo() {
        let s = parse("p:not(.skip) { color: red; }");
        let c = &s.rules[0].selectors[0].compounds[0];
        assert!(matches!(c.pseudo_classes[0], PseudoClass::Not(_)));
    }

    #[test]
    fn parses_nth_child_argument() {
        let cases = [
            ("p:nth-child(2n+1)", Nth { a: 2, b: 1 }),
            ("p:nth-child(odd)", Nth::ODD),
            ("p:nth-child(even)", Nth::EVEN),
            ("p:nth-child(3)", Nth { a: 0, b: 3 }),
            ("p:nth-child(-n+2)", Nth { a: -1, b: 2 }),
        ];
        for (src, want) in cases {
            let s = parse(&format!("{src} {{ color: red; }}"));
            let pc = &s.rules[0].selectors[0].compounds[0].pseudo_classes[0];
            match pc {
                PseudoClass::NthChild(got) => {
                    assert_eq!(got.a, want.a, "{src} a");
                    assert_eq!(got.b, want.b, "{src} b");
                }
                other => panic!("expected NthChild for {src}, got {other:?}"),
            }
        }
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
    fn at_media_preserves_top_level_rules() {
        // The @media block goes into `media_blocks`; sibling top-level
        // rules stay in `rules`. Used to be tested as "skipped" — they're
        // now collected so the cascade can pull them in conditionally.
        let s = parse("@media print { p { color: red; } } body { color: blue; }");
        assert_eq!(s.media_blocks.len(), 1);
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
