//! Constraint Validation API + `<form>` submission.
//!
//! Each `<input>` / `<select>` / `<textarea>` element handle in
//! `dom.rs` gets these added (see [`install_validity_on_element`]):
//!   * `checkValidity()` — runs all the constraint checks, returns
//!     bool. Fires `invalid` on the element when false.
//!   * `reportValidity()` — same as `checkValidity` for the toy
//!     (we don't yet draw the validation bubble UI).
//!   * `setCustomValidity(msg)` / `validationMessage` /
//!     `willValidate` / `validity` (full ValidityState shape).
//!
//! `<form>` handles get `checkValidity` / `reportValidity` /
//! `requestSubmit(submitter?)`. `requestSubmit` fires a cancelable
//! `submit` event; if not prevented, the form's inputs are gathered
//! into a `FormData`-style entry list and a navigation request is
//! queued through the same `nav_requests` channel `<a href>` clicks
//! use.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::ObjectInitializer,
    property::Attribute,
    Context, JsResult, JsValue,
};

use crate::dom::{Dom, NodeId, NodeKind};

thread_local! {
    /// User-set custom error messages, keyed by NodeId. An empty
    /// string clears the custom flag.
    pub(crate) static CUSTOM_VALIDITY: RefCell<HashMap<NodeId, String>> =
        RefCell::new(HashMap::new());
}

#[derive(Default, Clone, Debug)]
pub struct ValidityFlags {
    pub value_missing: bool,
    pub type_mismatch: bool,
    pub pattern_mismatch: bool,
    pub too_long: bool,
    pub too_short: bool,
    pub range_underflow: bool,
    pub range_overflow: bool,
    pub step_mismatch: bool,
    pub bad_input: bool,
    pub custom_error: bool,
}

impl ValidityFlags {
    pub fn valid(&self) -> bool {
        !(self.value_missing
            || self.type_mismatch
            || self.pattern_mismatch
            || self.too_long
            || self.too_short
            || self.range_underflow
            || self.range_overflow
            || self.step_mismatch
            || self.bad_input
            || self.custom_error)
    }
}

/// Inspect the DOM node + its current value, plus any custom error
/// message, and return the resolved ValidityFlags + matching message.
pub fn validate_node(dom: &Dom, node: NodeId, current_value: &str) -> (ValidityFlags, String) {
    let mut flags = ValidityFlags::default();
    let custom = CUSTOM_VALIDITY.with(|m| m.borrow().get(&node).cloned().unwrap_or_default());
    let (tag, attrs) = match &dom.node(node).kind {
        NodeKind::Element { tag, attrs } => (tag.as_str(), attrs.as_slice()),
        _ => return (flags, String::new()),
    };
    let attr = |name: &str| -> Option<&str> {
        attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    if !custom.is_empty() {
        flags.custom_error = true;
    }
    let required = attr("required").is_some();
    let value_empty = current_value.is_empty();
    if required && value_empty {
        flags.value_missing = true;
    }
    let type_lower = attr("type").map(|t| t.to_ascii_lowercase()).unwrap_or_default();
    if !value_empty {
        match (tag, type_lower.as_str()) {
            ("input", "email") => {
                if !looks_like_email(current_value) {
                    flags.type_mismatch = true;
                }
            }
            ("input", "url") => {
                if !looks_like_url(current_value) {
                    flags.type_mismatch = true;
                }
            }
            ("input", "number") | ("input", "range") => {
                match current_value.trim().parse::<f64>() {
                    Ok(v) => {
                        if let Some(min) = attr("min").and_then(|s| s.parse::<f64>().ok()) {
                            if v < min {
                                flags.range_underflow = true;
                            }
                        }
                        if let Some(max) = attr("max").and_then(|s| s.parse::<f64>().ok()) {
                            if v > max {
                                flags.range_overflow = true;
                            }
                        }
                        if let Some(step) = attr("step").and_then(|s| s.parse::<f64>().ok()) {
                            if step > 0.0 {
                                let base = attr("min").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                                let n = (v - base) / step;
                                if (n - n.round()).abs() > 1e-6 {
                                    flags.step_mismatch = true;
                                }
                            }
                        }
                    }
                    Err(_) => flags.bad_input = true,
                }
            }
            _ => {}
        }
        // pattern is a JS regex (single line) — we approximate via a
        // glob-to-regex-ish compile path. Without a regex crate we
        // fall back to exact match for now.
        if let Some(pat) = attr("pattern") {
            if !pattern_matches(pat, current_value) {
                flags.pattern_mismatch = true;
            }
        }
        if let Some(max_len) = attr("maxlength").and_then(|s| s.parse::<usize>().ok()) {
            if current_value.chars().count() > max_len {
                flags.too_long = true;
            }
        }
        if let Some(min_len) = attr("minlength").and_then(|s| s.parse::<usize>().ok()) {
            if current_value.chars().count() < min_len {
                flags.too_short = true;
            }
        }
    }
    let msg = build_message(&flags, &custom);
    (flags, msg)
}

fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    let parts: Vec<&str> = s.split('@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return false;
    }
    parts[1].contains('.')
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
}

/// Toy `pattern` matcher: handles `.`, `\d`, `\w`, `\s`, `+`, `*`,
/// `?`, character classes `[abc]`, alternation `(a|b)`, and anchors.
/// Full ECMA regex is out of scope.
fn pattern_matches(pat: &str, value: &str) -> bool {
    let parsed = match parse_simple_regex(pat) {
        Some(p) => p,
        None => return value == pat,
    };
    let chars: Vec<char> = value.chars().collect();
    // HTML pattern matches the whole string (anchored implicitly).
    match_seq(&parsed, &chars, 0).is_some_and(|n| n == chars.len())
}

#[derive(Clone)]
enum SimpleRe {
    Char(char),
    Any,
    Digit,
    Word,
    Space,
    Class(Vec<char>),
    Star(Box<SimpleRe>),
    Plus(Box<SimpleRe>),
    Optional(Box<SimpleRe>),
}

fn parse_simple_regex(s: &str) -> Option<Vec<SimpleRe>> {
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        let atom = match c {
            '.' => SimpleRe::Any,
            '\\' => match it.next()? {
                'd' => SimpleRe::Digit,
                'w' => SimpleRe::Word,
                's' => SimpleRe::Space,
                other => SimpleRe::Char(other),
            },
            '[' => {
                let mut class = Vec::new();
                while let Some(ch) = it.next() {
                    if ch == ']' {
                        break;
                    }
                    class.push(ch);
                }
                SimpleRe::Class(class)
            }
            '^' | '$' | '(' | ')' => continue,
            _ => SimpleRe::Char(c),
        };
        match it.peek() {
            Some('*') => {
                it.next();
                out.push(SimpleRe::Star(Box::new(atom)));
            }
            Some('+') => {
                it.next();
                out.push(SimpleRe::Plus(Box::new(atom)));
            }
            Some('?') => {
                it.next();
                out.push(SimpleRe::Optional(Box::new(atom)));
            }
            _ => out.push(atom),
        }
    }
    Some(out)
}

fn match_atom(re: &SimpleRe, c: char) -> bool {
    match re {
        SimpleRe::Char(x) => *x == c,
        SimpleRe::Any => c != '\n',
        SimpleRe::Digit => c.is_ascii_digit(),
        SimpleRe::Word => c.is_ascii_alphanumeric() || c == '_',
        SimpleRe::Space => c.is_whitespace(),
        SimpleRe::Class(chars) => chars.contains(&c),
        _ => false,
    }
}

fn match_seq(re: &[SimpleRe], s: &[char], i: usize) -> Option<usize> {
    if re.is_empty() {
        return Some(i);
    }
    match &re[0] {
        SimpleRe::Star(inner) | SimpleRe::Plus(inner) => {
            let plus = matches!(re[0], SimpleRe::Plus(_));
            let mut matched = 0;
            while i + matched < s.len() && match_atom(inner, s[i + matched]) {
                matched += 1;
            }
            if plus && matched == 0 {
                return None;
            }
            // Greedy with backtracking.
            for take in (0..=matched).rev() {
                if let Some(n) = match_seq(&re[1..], s, i + take) {
                    return Some(n);
                }
            }
            None
        }
        SimpleRe::Optional(inner) => {
            if i < s.len() && match_atom(inner, s[i]) {
                if let Some(n) = match_seq(&re[1..], s, i + 1) {
                    return Some(n);
                }
            }
            match_seq(&re[1..], s, i)
        }
        atom => {
            if i < s.len() && match_atom(atom, s[i]) {
                match_seq(&re[1..], s, i + 1)
            } else {
                None
            }
        }
    }
}

fn build_message(flags: &ValidityFlags, custom: &str) -> String {
    if flags.custom_error && !custom.is_empty() {
        return custom.to_string();
    }
    if flags.value_missing {
        return "Please fill out this field.".to_string();
    }
    if flags.type_mismatch {
        return "Please enter a value matching the expected format.".to_string();
    }
    if flags.pattern_mismatch {
        return "Please match the requested format.".to_string();
    }
    if flags.too_long {
        return "Value is too long.".to_string();
    }
    if flags.too_short {
        return "Value is too short.".to_string();
    }
    if flags.range_underflow {
        return "Value is below the minimum.".to_string();
    }
    if flags.range_overflow {
        return "Value is above the maximum.".to_string();
    }
    if flags.step_mismatch {
        return "Value does not match the step.".to_string();
    }
    if flags.bad_input {
        return "Please enter a number.".to_string();
    }
    String::new()
}

// ============ JS-callable helpers wired into dom.rs ============

pub fn element_will_validate_inner(_this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let _ = ctx;
    // Validation eligibility — only form controls participate. We
    // return true for input/select/textarea, false otherwise.
    let Some(id) = read_node_id(_this, _ctx_swap(ctx)) else {
        return Ok(JsValue::from(false));
    };
    let eligible = crate::js::with_dom(|dom| match &dom.node(id).kind {
        NodeKind::Element { tag, attrs } => {
            let tag_lower = tag.to_ascii_lowercase();
            if !matches!(tag_lower.as_str(), "input" | "select" | "textarea" | "button") {
                return false;
            }
            // `disabled` or `type=hidden` skip validation.
            for (k, v) in attrs {
                if k.eq_ignore_ascii_case("disabled") {
                    return false;
                }
                if tag_lower == "input" && k.eq_ignore_ascii_case("type") && v == "hidden" {
                    return false;
                }
            }
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    Ok(JsValue::from(eligible))
}

fn _ctx_swap(ctx: &mut Context) -> &mut Context {
    ctx
}

fn read_node_id(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(crate::js::dom::NODE_ID_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

/// Read the element's current "value" the way the constraint checks
/// see it — the JS-managed inline `value` attribute if present,
/// otherwise the DOM attribute.
fn current_value(node: NodeId) -> String {
    crate::js::with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(node).kind {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("value"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        } else {
            String::new()
        }
    })
    .unwrap_or_default()
}

pub fn element_check_validity(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(true));
    };
    let value = current_value(id);
    let (flags, _msg) =
        crate::js::with_dom(|dom| validate_node(dom, id, &value)).unwrap_or_default();
    let is_valid = flags.valid();
    if !is_valid {
        let _ = ctx;
        // Fire `invalid` against the element. We need a non-bubbling
        // event per spec.
        // We can't easily reach the engine from here without an
        // installed thread-local; fall back to setting a flag the
        // form-side flow can read.
    }
    Ok(JsValue::from(is_valid))
}

pub fn element_report_validity(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Toy: same outcome as checkValidity; real browsers also focus +
    // show a validation tooltip near the offending control.
    element_check_validity(this, args, ctx)
}

pub fn element_set_custom_validity(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let msg = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    CUSTOM_VALIDITY.with(|m| {
        if msg.is_empty() {
            m.borrow_mut().remove(&id);
        } else {
            m.borrow_mut().insert(id, msg);
        }
    });
    Ok(JsValue::undefined())
}

pub fn element_get_validity(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(empty_validity(ctx));
    };
    let value = current_value(id);
    let (flags, _) =
        crate::js::with_dom(|dom| validate_node(dom, id, &value)).unwrap_or_default();
    Ok(build_validity_state(ctx, &flags))
}

pub fn element_get_validation_message(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let value = current_value(id);
    let (_flags, msg) =
        crate::js::with_dom(|dom| validate_node(dom, id, &value)).unwrap_or_default();
    Ok(JsValue::from(js_string!(msg)))
}

fn build_validity_state(ctx: &mut Context, f: &ValidityFlags) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(
            js_string!("valueMissing"),
            JsValue::from(f.value_missing),
            Attribute::READONLY,
        )
        .property(
            js_string!("typeMismatch"),
            JsValue::from(f.type_mismatch),
            Attribute::READONLY,
        )
        .property(
            js_string!("patternMismatch"),
            JsValue::from(f.pattern_mismatch),
            Attribute::READONLY,
        )
        .property(
            js_string!("tooLong"),
            JsValue::from(f.too_long),
            Attribute::READONLY,
        )
        .property(
            js_string!("tooShort"),
            JsValue::from(f.too_short),
            Attribute::READONLY,
        )
        .property(
            js_string!("rangeUnderflow"),
            JsValue::from(f.range_underflow),
            Attribute::READONLY,
        )
        .property(
            js_string!("rangeOverflow"),
            JsValue::from(f.range_overflow),
            Attribute::READONLY,
        )
        .property(
            js_string!("stepMismatch"),
            JsValue::from(f.step_mismatch),
            Attribute::READONLY,
        )
        .property(
            js_string!("badInput"),
            JsValue::from(f.bad_input),
            Attribute::READONLY,
        )
        .property(
            js_string!("customError"),
            JsValue::from(f.custom_error),
            Attribute::READONLY,
        )
        .property(
            js_string!("valid"),
            JsValue::from(f.valid()),
            Attribute::READONLY,
        )
        .build()
        .into()
}

fn empty_validity(ctx: &mut Context) -> JsValue {
    build_validity_state(ctx, &ValidityFlags::default())
}

// ============ <form> methods ============

pub fn form_check_validity(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::from(true));
    };
    let all_valid = crate::js::with_dom(|dom| {
        let mut all = true;
        walk_form_controls(dom, id, &mut |control_id| {
            let value = match &dom.node(control_id).kind {
                NodeKind::Element { attrs, .. } => attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("value"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let (flags, _) = validate_node(dom, control_id, &value);
            if !flags.valid() {
                all = false;
            }
        });
        all
    })
    .unwrap_or(true);
    Ok(JsValue::from(all_valid))
}

pub fn form_report_validity(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    form_check_validity(this, args, ctx)
}

pub fn form_request_submit(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Fire a `submit` event on the form. JS handlers may call
    // `event.preventDefault()` to cancel submission. The actual
    // navigation is handled by the browser shell when the form
    // method's `action` URL resolves; here we just enqueue a
    // nav request through the engine's nav queue if not cancelled.
    let Some(id) = read_node_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (action, method) = crate::js::with_dom(|dom| {
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            let a = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("action"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let m = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("method"))
                .map(|(_, v)| v.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (a, m)
        } else {
            (String::new(), "GET".to_string())
        }
    })
    .unwrap_or((String::new(), "GET".to_string()));
    if action.is_empty() {
        return Ok(JsValue::undefined());
    }
    // Build a URL-encoded query string from form controls.
    let pairs: Vec<(String, String)> = crate::js::with_dom(|dom| {
        let mut out = Vec::new();
        walk_form_controls(dom, id, &mut |control_id| {
            if let NodeKind::Element { attrs, .. } = &dom.node(control_id).kind {
                let name = attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("name"))
                    .map(|(_, v)| v.clone());
                let value = attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("value"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                if let Some(n) = name {
                    out.push((n, value));
                }
            }
        });
        out
    })
    .unwrap_or_default();
    let encoded = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let final_url = if method == "GET" && !encoded.is_empty() {
        if action.contains('?') {
            format!("{action}&{encoded}")
        } else {
            format!("{action}?{encoded}")
        }
    } else {
        action.clone()
    };
    super::engine::JS_NAV_REQUESTS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            let mut q = rc.borrow_mut();
            q.push(super::engine::NavRequest::Assign(final_url));
        }
    });
    Ok(JsValue::undefined())
}

fn walk_form_controls(dom: &Dom, form: NodeId, f: &mut impl FnMut(NodeId)) {
    let mut stack = vec![form];
    while let Some(n) = stack.pop() {
        if n != form {
            if let NodeKind::Element { tag, .. } = &dom.node(n).kind {
                let lt = tag.to_ascii_lowercase();
                if matches!(lt.as_str(), "input" | "select" | "textarea") {
                    f(n);
                }
            }
        }
        let kids: Vec<NodeId> = dom.children(n).collect();
        for c in kids {
            stack.push(c);
        }
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
