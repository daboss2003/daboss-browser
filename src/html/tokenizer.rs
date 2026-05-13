//! HTML5 tokenizer.
//!
//! Implements a pragmatic subset of the WHATWG state machine — roughly 25 of
//! the spec's 80-odd states, enough to chew through real-world pages.
//! Skipped: PLAINTEXT, RCDATA character references, CDATA, named entities
//! beyond a small set, error recovery edge cases.
//!
//! The state machine processes one char at a time, mutating internal buffers
//! and pushing finished tokens into a queue. `next_token` drains that queue.
//! Raw-text elements (script / style / etc.) shortcut out of the state machine
//! and bulk-scan until the matching end tag.

use std::mem;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Doctype {
        name: String,
    },
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Text(String),
    Comment(String),
}

pub struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
    state: State,
    text_buf: String,
    tag: TagBuilder,
    comment_buf: String,
    doctype_buf: String,
    pending: Vec<Token>,
    raw_text_for: Option<String>,
}

#[derive(Default)]
struct TagBuilder {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
    is_end: bool,
    current_attr_name: String,
    current_attr_value: String,
}

#[derive(Debug)]
enum State {
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    MarkupDeclarationOpen,
    CommentStart,
    Comment,
    CommentEndDash,
    CommentEnd,
    BogusComment,
    DoctypeStart,
    BeforeDoctypeName,
    DoctypeName,
    BogusDoctype,
}

impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            state: State::Data,
            text_buf: String::new(),
            tag: TagBuilder::default(),
            comment_buf: String::new(),
            doctype_buf: String::new(),
            pending: Vec::new(),
            raw_text_for: None,
        }
    }

    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some(token) = self.next_token() {
            tokens.push(token);
        }
        tokens
    }

    pub fn next_token(&mut self) -> Option<Token> {
        loop {
            if !self.pending.is_empty() {
                return Some(self.pending.remove(0));
            }
            if let Some(end_tag) = self.raw_text_for.clone() {
                self.scan_raw_text(&end_tag);
                continue;
            }
            match self.peek() {
                Some(c) => self.step(c),
                None => {
                    self.flush_text();
                    return if self.pending.is_empty() {
                        None
                    } else {
                        Some(self.pending.remove(0))
                    };
                }
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self, c: char) {
        self.pos += c.len_utf8();
    }

    fn step(&mut self, c: char) {
        match self.state {
            State::Data => match c {
                '<' => {
                    self.flush_text();
                    self.advance(c);
                    self.state = State::TagOpen;
                }
                '&' => {
                    self.advance(c);
                    if let Some(decoded) = self.consume_entity() {
                        self.text_buf.push_str(&decoded);
                    } else {
                        self.text_buf.push('&');
                    }
                }
                _ => {
                    self.text_buf.push(c);
                    self.advance(c);
                }
            },
            State::TagOpen => match c {
                '/' => {
                    self.advance(c);
                    self.state = State::EndTagOpen;
                }
                '!' => {
                    self.advance(c);
                    self.state = State::MarkupDeclarationOpen;
                }
                c if c.is_ascii_alphabetic() => {
                    self.tag = TagBuilder::default();
                    self.state = State::TagName;
                }
                _ => {
                    self.text_buf.push('<');
                    self.state = State::Data;
                }
            },
            State::EndTagOpen => {
                if c.is_ascii_alphabetic() {
                    self.tag = TagBuilder {
                        is_end: true,
                        ..TagBuilder::default()
                    };
                    self.state = State::TagName;
                } else {
                    self.text_buf.push_str("</");
                    self.advance(c);
                    self.state = State::Data;
                }
            }
            State::TagName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                    self.state = State::BeforeAttributeName;
                } else if c == '/' {
                    self.advance(c);
                    self.state = State::SelfClosingStartTag;
                } else if c == '>' {
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.tag.name.push(c.to_ascii_lowercase());
                    self.advance(c);
                }
            }
            State::BeforeAttributeName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                } else if c == '/' {
                    self.advance(c);
                    self.state = State::SelfClosingStartTag;
                } else if c == '>' {
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.tag.current_attr_name.clear();
                    self.tag.current_attr_value.clear();
                    self.state = State::AttributeName;
                }
            }
            State::AttributeName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                    self.state = State::AfterAttributeName;
                } else if c == '=' {
                    self.advance(c);
                    self.state = State::BeforeAttributeValue;
                } else if c == '/' || c == '>' {
                    self.commit_attr();
                    self.state = State::BeforeAttributeName;
                } else {
                    self.tag.current_attr_name.push(c.to_ascii_lowercase());
                    self.advance(c);
                }
            }
            State::AfterAttributeName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                } else if c == '/' {
                    self.commit_attr();
                    self.advance(c);
                    self.state = State::SelfClosingStartTag;
                } else if c == '=' {
                    self.advance(c);
                    self.state = State::BeforeAttributeValue;
                } else if c == '>' {
                    self.commit_attr();
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.commit_attr();
                    self.tag.current_attr_name.clear();
                    self.tag.current_attr_value.clear();
                    self.state = State::AttributeName;
                }
            }
            State::BeforeAttributeValue => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                } else if c == '"' {
                    self.advance(c);
                    self.state = State::AttributeValueDoubleQuoted;
                } else if c == '\'' {
                    self.advance(c);
                    self.state = State::AttributeValueSingleQuoted;
                } else if c == '>' {
                    self.commit_attr();
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.state = State::AttributeValueUnquoted;
                }
            }
            State::AttributeValueDoubleQuoted => match c {
                '"' => {
                    self.commit_attr();
                    self.advance(c);
                    self.state = State::AfterAttributeValueQuoted;
                }
                '&' => {
                    self.advance(c);
                    if let Some(decoded) = self.consume_entity() {
                        self.tag.current_attr_value.push_str(&decoded);
                    } else {
                        self.tag.current_attr_value.push('&');
                    }
                }
                _ => {
                    self.tag.current_attr_value.push(c);
                    self.advance(c);
                }
            },
            State::AttributeValueSingleQuoted => match c {
                '\'' => {
                    self.commit_attr();
                    self.advance(c);
                    self.state = State::AfterAttributeValueQuoted;
                }
                '&' => {
                    self.advance(c);
                    if let Some(decoded) = self.consume_entity() {
                        self.tag.current_attr_value.push_str(&decoded);
                    } else {
                        self.tag.current_attr_value.push('&');
                    }
                }
                _ => {
                    self.tag.current_attr_value.push(c);
                    self.advance(c);
                }
            },
            State::AttributeValueUnquoted => {
                if c.is_ascii_whitespace() {
                    self.commit_attr();
                    self.advance(c);
                    self.state = State::BeforeAttributeName;
                } else if c == '>' {
                    self.commit_attr();
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else if c == '&' {
                    self.advance(c);
                    if let Some(decoded) = self.consume_entity() {
                        self.tag.current_attr_value.push_str(&decoded);
                    } else {
                        self.tag.current_attr_value.push('&');
                    }
                } else {
                    self.tag.current_attr_value.push(c);
                    self.advance(c);
                }
            }
            State::AfterAttributeValueQuoted => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                    self.state = State::BeforeAttributeName;
                } else if c == '/' {
                    self.advance(c);
                    self.state = State::SelfClosingStartTag;
                } else if c == '>' {
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.state = State::BeforeAttributeName;
                }
            }
            State::SelfClosingStartTag => {
                if c == '>' {
                    self.tag.self_closing = true;
                    self.advance(c);
                    self.emit_tag();
                    self.state = State::Data;
                } else {
                    self.state = State::BeforeAttributeName;
                }
            }
            State::MarkupDeclarationOpen => {
                let rest = &self.input[self.pos..];
                if rest.starts_with("--") {
                    self.pos += 2;
                    self.comment_buf.clear();
                    self.state = State::CommentStart;
                } else if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case("DOCTYPE") {
                    self.pos += 7;
                    self.doctype_buf.clear();
                    self.state = State::DoctypeStart;
                } else {
                    self.comment_buf.clear();
                    self.state = State::BogusComment;
                }
            }
            State::CommentStart => {
                self.state = State::Comment;
            }
            State::Comment => {
                if c == '-' {
                    self.advance(c);
                    self.state = State::CommentEndDash;
                } else {
                    self.comment_buf.push(c);
                    self.advance(c);
                }
            }
            State::CommentEndDash => {
                if c == '-' {
                    self.advance(c);
                    self.state = State::CommentEnd;
                } else {
                    self.comment_buf.push('-');
                    self.state = State::Comment;
                }
            }
            State::CommentEnd => {
                if c == '>' {
                    self.advance(c);
                    let text = mem::take(&mut self.comment_buf);
                    self.pending.push(Token::Comment(text));
                    self.state = State::Data;
                } else if c == '-' {
                    self.comment_buf.push('-');
                    self.advance(c);
                } else {
                    self.comment_buf.push_str("--");
                    self.state = State::Comment;
                }
            }
            State::BogusComment => {
                if c == '>' {
                    self.advance(c);
                    let text = mem::take(&mut self.comment_buf);
                    self.pending.push(Token::Comment(text));
                    self.state = State::Data;
                } else {
                    self.comment_buf.push(c);
                    self.advance(c);
                }
            }
            State::DoctypeStart => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                }
                self.state = State::BeforeDoctypeName;
            }
            State::BeforeDoctypeName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                } else if c == '>' {
                    self.advance(c);
                    self.pending.push(Token::Doctype {
                        name: String::new(),
                    });
                    self.state = State::Data;
                } else {
                    self.doctype_buf.push(c.to_ascii_lowercase());
                    self.advance(c);
                    self.state = State::DoctypeName;
                }
            }
            State::DoctypeName => {
                if c.is_ascii_whitespace() {
                    self.advance(c);
                    self.state = State::BogusDoctype;
                } else if c == '>' {
                    self.advance(c);
                    let name = mem::take(&mut self.doctype_buf);
                    self.pending.push(Token::Doctype { name });
                    self.state = State::Data;
                } else {
                    self.doctype_buf.push(c.to_ascii_lowercase());
                    self.advance(c);
                }
            }
            State::BogusDoctype => {
                if c == '>' {
                    self.advance(c);
                    let name = mem::take(&mut self.doctype_buf);
                    self.pending.push(Token::Doctype { name });
                    self.state = State::Data;
                } else {
                    self.advance(c);
                }
            }
        }
    }

    /// Bulk-scan the input for the matching `</tagname>` of a raw-text element.
    /// Everything before it becomes a single Text token; the end tag becomes
    /// an EndTag token; we return to normal Data state.
    fn scan_raw_text(&mut self, end_tag: &str) {
        let mut text = String::new();
        while self.pos < self.input.len() {
            let rest = &self.input[self.pos..];
            match rest.find('<') {
                None => {
                    text.push_str(rest);
                    self.pos = self.input.len();
                    break;
                }
                Some(0) => {
                    let after_lt = &rest[1..];
                    let is_end_tag = after_lt.starts_with('/')
                        && after_lt.len() > 1 + end_tag.len()
                        && after_lt[1..1 + end_tag.len()].eq_ignore_ascii_case(end_tag)
                        && after_lt[1 + end_tag.len()..]
                            .chars()
                            .next()
                            .map_or(true, |c| !c.is_ascii_alphabetic());
                    if is_end_tag {
                        if !text.is_empty() {
                            self.pending.push(Token::Text(text));
                        }
                        self.pos += 2 + end_tag.len(); // skip "</tagname"
                        // Skip optional whitespace, attributes, until '>'
                        while let Some(c) = self.peek() {
                            self.advance(c);
                            if c == '>' {
                                break;
                            }
                        }
                        self.pending.push(Token::EndTag {
                            name: end_tag.to_string(),
                        });
                        self.raw_text_for = None;
                        return;
                    }
                    text.push('<');
                    self.pos += 1;
                }
                Some(n) => {
                    text.push_str(&rest[..n]);
                    self.pos += n;
                }
            }
        }
        if !text.is_empty() {
            self.pending.push(Token::Text(text));
        }
        self.raw_text_for = None;
    }

    fn commit_attr(&mut self) {
        if !self.tag.current_attr_name.is_empty() {
            let name = mem::take(&mut self.tag.current_attr_name);
            let value = mem::take(&mut self.tag.current_attr_value);
            self.tag.attrs.push((name, value));
        }
    }

    fn emit_tag(&mut self) {
        self.commit_attr();
        let tag = mem::take(&mut self.tag);
        if tag.is_end {
            self.pending.push(Token::EndTag { name: tag.name });
            self.raw_text_for = None;
        } else {
            let is_raw = matches!(
                tag.name.as_str(),
                "script" | "style" | "textarea" | "title"
            );
            let name = tag.name.clone();
            self.pending.push(Token::StartTag {
                name: tag.name,
                attrs: tag.attrs,
                self_closing: tag.self_closing,
            });
            if is_raw && !tag.self_closing {
                self.raw_text_for = Some(name);
            }
        }
    }

    fn flush_text(&mut self) {
        if !self.text_buf.is_empty() {
            let text = mem::take(&mut self.text_buf);
            self.pending.push(Token::Text(text));
        }
    }

    /// Consume a character reference starting after the `&`. Returns the
    /// decoded string, or `None` if we couldn't recognize it (caller emits
    /// the literal `&`).
    fn consume_entity(&mut self) -> Option<String> {
        let rest = &self.input[self.pos..];

        // Numeric: &#NNN; or &#xHHH;
        if let Some(after_hash) = rest.strip_prefix('#') {
            let (radix, digits_start) =
                if after_hash.starts_with('x') || after_hash.starts_with('X') {
                    (16, 2)
                } else {
                    (10, 1)
                };
            let digits = &rest[digits_start..];
            let end = digits
                .find(|c: char| !c.is_digit(radix))
                .unwrap_or(digits.len());
            if end > 0 {
                if let Ok(n) = u32::from_str_radix(&digits[..end], radix) {
                    if let Some(c) = char::from_u32(n) {
                        let mut consumed = digits_start + end;
                        if rest[consumed..].starts_with(';') {
                            consumed += 1;
                        }
                        self.pos += consumed;
                        return Some(c.to_string());
                    }
                }
            }
            return None;
        }

        // Named (very small allowlist; uncovered entities fall through).
        for &(name, repl) in NAMED_ENTITIES {
            if rest.starts_with(name) && rest[name.len()..].starts_with(';') {
                self.pos += name.len() + 1;
                return Some(repl.to_string());
            }
        }
        None
    }
}

const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{00a0}"),
    ("copy", "\u{00a9}"),
    ("reg", "\u{00ae}"),
    ("trade", "\u{2122}"),
    ("mdash", "\u{2014}"),
    ("ndash", "\u{2013}"),
    ("hellip", "\u{2026}"),
    ("lsquo", "\u{2018}"),
    ("rsquo", "\u{2019}"),
    ("ldquo", "\u{201c}"),
    ("rdquo", "\u{201d}"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(input: &str) -> Vec<Token> {
        Tokenizer::new(input).tokenize()
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(tok("").is_empty());
    }

    #[test]
    fn plain_text() {
        assert_eq!(tok("hello"), vec![Token::Text("hello".into())]);
    }

    #[test]
    fn simple_element() {
        assert_eq!(
            tok("<p>hi</p>"),
            vec![
                Token::StartTag {
                    name: "p".into(),
                    attrs: vec![],
                    self_closing: false
                },
                Token::Text("hi".into()),
                Token::EndTag { name: "p".into() },
            ]
        );
    }

    #[test]
    fn attribute_quoted_and_unquoted() {
        let t = tok(r#"<a href="x" class=y id='z'>"#);
        assert_eq!(
            t,
            vec![Token::StartTag {
                name: "a".into(),
                attrs: vec![
                    ("href".into(), "x".into()),
                    ("class".into(), "y".into()),
                    ("id".into(), "z".into()),
                ],
                self_closing: false,
            }]
        );
    }

    #[test]
    fn self_closing() {
        let t = tok("<br/>");
        assert_eq!(
            t,
            vec![Token::StartTag {
                name: "br".into(),
                attrs: vec![],
                self_closing: true
            }]
        );
    }

    #[test]
    fn comment() {
        let t = tok("<!-- hi -->");
        assert_eq!(t, vec![Token::Comment(" hi ".into())]);
    }

    #[test]
    fn doctype() {
        let t = tok("<!DOCTYPE html>");
        assert_eq!(t, vec![Token::Doctype { name: "html".into() }]);
    }

    #[test]
    fn entities_in_text() {
        let t = tok("a &amp; b &lt; c &#65; &#x41;");
        assert_eq!(t, vec![Token::Text("a & b < c A A".into())]);
    }

    #[test]
    fn entity_in_attr() {
        let t = tok(r#"<a href="x&amp;y">"#);
        assert_eq!(
            t,
            vec![Token::StartTag {
                name: "a".into(),
                attrs: vec![("href".into(), "x&y".into())],
                self_closing: false
            }]
        );
    }

    #[test]
    fn script_is_raw_text() {
        let t = tok(r#"<script>var x = "<p>";</script>"#);
        assert_eq!(
            t,
            vec![
                Token::StartTag {
                    name: "script".into(),
                    attrs: vec![],
                    self_closing: false
                },
                Token::Text(r#"var x = "<p>";"#.into()),
                Token::EndTag {
                    name: "script".into()
                },
            ]
        );
    }

    #[test]
    fn case_insensitive_tag_names() {
        let t = tok("<DIV CLASS=Foo>");
        assert_eq!(
            t,
            vec![Token::StartTag {
                name: "div".into(),
                attrs: vec![("class".into(), "Foo".into())],
                self_closing: false
            }]
        );
    }
}
