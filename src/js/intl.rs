//! `Intl.*` — minimal C-locale implementation.
//!
//! Surface (every constructor matches the spec's call shape):
//!   * `Intl.NumberFormat(locale?, options?)` with `.format(n)` /
//!     `.formatToParts(n)`.
//!   * `Intl.DateTimeFormat(locale?, options?)` with `.format(d)` /
//!     `.formatToParts(d)`.
//!   * `Intl.Collator(locale?, options?)` with `.compare(a, b)`.
//!   * `Intl.PluralRules(locale?, options?)` with `.select(n)`.
//!   * `Intl.ListFormat(locale?, options?)` with `.format(list)`.
//!   * `Intl.RelativeTimeFormat(locale?, options?)` with
//!     `.format(value, unit)`.
//!   * `Intl.DisplayNames(locale?, options?)` with `.of(code)`.
//!   * `Intl.Locale(tag, options?)` — stores the tag, exposes
//!     `baseName`, `language`, `region`, etc.
//!   * `Intl.Segmenter(locale?, options?)` with `.segment(str)`.
//!
//! Everything formats as if the locale were `en-US` because we don't
//! ship ICU data. The point is that pages don't crash on
//! `Intl.NumberFormat(...).format(n)` and similar.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction,
};

#[derive(Default, Clone)]
struct NumberFormatOpts {
    style: String,             // "decimal" / "currency" / "percent" / "unit"
    currency: Option<String>,  // ISO code when style == currency
    minimum_fraction_digits: Option<u32>,
    maximum_fraction_digits: Option<u32>,
    use_grouping: bool,
}

#[derive(Default, Clone)]
struct DateTimeFormatOpts {
    year: Option<String>,
    month: Option<String>,
    day: Option<String>,
    hour: Option<String>,
    minute: Option<String>,
    second: Option<String>,
    weekday: Option<String>,
}

thread_local! {
    static NF_OPTS: RefCell<HashMap<u32, NumberFormatOpts>> = RefCell::new(HashMap::new());
    static DT_OPTS: RefCell<HashMap<u32, DateTimeFormatOpts>> = RefCell::new(HashMap::new());
    static NEXT_INTL_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    NEXT_INTL_ID.with(|n| {
        let mut v = n.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };

    let nf = mk(intl_number_format_ctor);
    let dt = mk(intl_datetime_format_ctor);
    let coll = mk(intl_collator_ctor);
    let pr = mk(intl_plural_rules_ctor);
    let lf = mk(intl_list_format_ctor);
    let rtf = mk(intl_relative_time_format_ctor);
    let dn = mk(intl_display_names_ctor);
    let loc = mk(intl_locale_ctor);
    let seg = mk(intl_segmenter_ctor);
    let supported = mk(intl_supported_locales_of);

    let intl = ObjectInitializer::new(ctx)
        .property(js_string!("NumberFormat"), JsValue::from(nf), Attribute::READONLY)
        .property(js_string!("DateTimeFormat"), JsValue::from(dt), Attribute::READONLY)
        .property(js_string!("Collator"), JsValue::from(coll), Attribute::READONLY)
        .property(js_string!("PluralRules"), JsValue::from(pr), Attribute::READONLY)
        .property(js_string!("ListFormat"), JsValue::from(lf), Attribute::READONLY)
        .property(
            js_string!("RelativeTimeFormat"),
            JsValue::from(rtf),
            Attribute::READONLY,
        )
        .property(js_string!("DisplayNames"), JsValue::from(dn), Attribute::READONLY)
        .property(js_string!("Locale"), JsValue::from(loc), Attribute::READONLY)
        .property(js_string!("Segmenter"), JsValue::from(seg), Attribute::READONLY)
        .property(
            js_string!("getCanonicalLocales"),
            JsValue::from(supported.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("supportedValuesOf"),
            JsValue::from(supported),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("Intl"),
        intl,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn intl_supported_locales_of(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    if let Some(v) = args.first() {
        if let Ok(s) = v.to_string(ctx) {
            let _ = arr.push(JsValue::from(js_string!(s.to_std_string_escaped())), ctx);
        }
    }
    Ok(arr.into())
}

// =================== Intl.NumberFormat ===================

fn intl_number_format_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let opts_obj = args.get(1).and_then(|v| v.as_object().cloned());
    let mut opts = NumberFormatOpts {
        style: "decimal".into(),
        use_grouping: true,
        ..Default::default()
    };
    if let Some(o) = &opts_obj {
        if let Ok(v) = o.get(js_string!("style"), ctx) {
            if let Ok(s) = v.to_string(ctx) {
                opts.style = s.to_std_string_escaped();
            }
        }
        if let Ok(v) = o.get(js_string!("currency"), ctx) {
            if let Ok(s) = v.to_string(ctx) {
                let cur = s.to_std_string_escaped();
                if !cur.is_empty() {
                    opts.currency = Some(cur);
                }
            }
        }
        if let Ok(v) = o.get(js_string!("minimumFractionDigits"), ctx) {
            if let Ok(n) = v.to_u32(ctx) {
                opts.minimum_fraction_digits = Some(n);
            }
        }
        if let Ok(v) = o.get(js_string!("maximumFractionDigits"), ctx) {
            if let Ok(n) = v.to_u32(ctx) {
                opts.maximum_fraction_digits = Some(n);
            }
        }
        if let Ok(v) = o.get(js_string!("useGrouping"), ctx) {
            opts.use_grouping = v.to_boolean();
        }
    }
    let id = next_id();
    NF_OPTS.with(|m| m.borrow_mut().insert(id, opts));
    Ok(JsValue::from(build_nf_object(ctx, id)))
}

fn build_nf_object(ctx: &mut Context, id: u32) -> JsObject {
    ObjectInitializer::new(ctx)
        .property(js_string!("__intl_id"), JsValue::from(id), Attribute::READONLY)
        .function(NativeFunction::from_fn_ptr(nf_format), js_string!("format"), 1)
        .function(
            NativeFunction::from_fn_ptr(nf_format_to_parts),
            js_string!("formatToParts"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(resolved_options_stub),
            js_string!("resolvedOptions"),
            0,
        )
        .build()
}

fn read_intl_id(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    this.as_object()
        .and_then(|o| o.get(js_string!("__intl_id"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
}

fn nf_format(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = read_intl_id(this, ctx).unwrap_or(0);
    let opts = NF_OPTS.with(|m| m.borrow().get(&id).cloned()).unwrap_or_default();
    let n = args.first().map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    Ok(JsValue::from(js_string!(format_number(n, &opts))))
}

fn nf_format_to_parts(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Spec returns granular parts; we return one "literal" chunk
    // which is enough for the common feature-detection pattern.
    let arr = JsArray::new(ctx);
    let s = nf_format(this, args, ctx)?.to_string(ctx)?.to_std_string_escaped();
    let part = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!("literal")),
            Attribute::READONLY,
        )
        .property(
            js_string!("value"),
            JsValue::from(js_string!(s)),
            Attribute::READONLY,
        )
        .build();
    let _ = arr.push(JsValue::from(part), ctx);
    Ok(arr.into())
}

fn resolved_options_stub(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let o = ObjectInitializer::new(ctx)
        .property(
            js_string!("locale"),
            JsValue::from(js_string!("en-US")),
            Attribute::READONLY,
        )
        .build();
    Ok(JsValue::from(o))
}

fn format_number(n: f64, opts: &NumberFormatOpts) -> String {
    let mut frac = match (opts.minimum_fraction_digits, opts.maximum_fraction_digits) {
        (Some(_), Some(max)) => max as usize,
        (Some(min), None) => min as usize,
        (None, Some(max)) => max as usize,
        (None, None) => {
            if opts.style == "currency" {
                2
            } else {
                3
            }
        }
    };
    let min_frac = opts.minimum_fraction_digits.unwrap_or(0) as usize;
    if frac < min_frac {
        frac = min_frac;
    }

    let scaled = if opts.style == "percent" { n * 100.0 } else { n };
    let mut s = format!("{:.*}", frac, scaled);
    if opts.use_grouping {
        s = group_thousands(&s);
    }
    match opts.style.as_str() {
        "currency" => {
            let symbol = opts.currency.as_deref().unwrap_or("$");
            let glyph = currency_glyph(symbol);
            if glyph.is_empty() {
                format!("{symbol} {s}")
            } else {
                format!("{glyph}{s}")
            }
        }
        "percent" => format!("{s}%"),
        _ => s,
    }
}

fn currency_glyph(code: &str) -> &'static str {
    match code.to_ascii_uppercase().as_str() {
        "USD" | "CAD" | "AUD" | "NZD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" | "CNY" => "¥",
        "INR" => "₹",
        _ => "",
    }
}

fn group_thousands(s: &str) -> String {
    let (int_part, rest) = match s.find('.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let negative = int_part.starts_with('-');
    let digits = if negative { &int_part[1..] } else { int_part };
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    let bytes = digits.as_bytes();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if negative {
        format!("-{grouped}{rest}")
    } else {
        format!("{grouped}{rest}")
    }
}

// =================== Intl.DateTimeFormat ===================

fn intl_datetime_format_ctor(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let opts_obj = args.get(1).and_then(|v| v.as_object().cloned());
    let mut opts = DateTimeFormatOpts::default();
    if let Some(o) = &opts_obj {
        let pick = |obj: &JsObject, key: &str, ctx: &mut Context| -> Option<String> {
            obj.get(js_string!(key.to_string()), ctx)
                .ok()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
        };
        opts.year = pick(o, "year", ctx);
        opts.month = pick(o, "month", ctx);
        opts.day = pick(o, "day", ctx);
        opts.hour = pick(o, "hour", ctx);
        opts.minute = pick(o, "minute", ctx);
        opts.second = pick(o, "second", ctx);
        opts.weekday = pick(o, "weekday", ctx);
    }
    let id = next_id();
    DT_OPTS.with(|m| m.borrow_mut().insert(id, opts));
    Ok(JsValue::from(build_dt_object(ctx, id)))
}

fn build_dt_object(ctx: &mut Context, id: u32) -> JsObject {
    ObjectInitializer::new(ctx)
        .property(js_string!("__intl_id"), JsValue::from(id), Attribute::READONLY)
        .function(NativeFunction::from_fn_ptr(dt_format), js_string!("format"), 1)
        .function(
            NativeFunction::from_fn_ptr(dt_format_to_parts),
            js_string!("formatToParts"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(resolved_options_stub),
            js_string!("resolvedOptions"),
            0,
        )
        .build()
}

fn dt_format(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = read_intl_id(this, ctx).unwrap_or(0);
    let opts = DT_OPTS.with(|m| m.borrow().get(&id).cloned()).unwrap_or_default();
    let ms = args
        .first()
        .map(|v| extract_date_ms(v, ctx))
        .transpose()?
        .unwrap_or(0.0) as i64;
    Ok(JsValue::from(js_string!(format_date(ms, &opts))))
}

fn dt_format_to_parts(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    let s = dt_format(this, args, ctx)?.to_string(ctx)?.to_std_string_escaped();
    let part = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!("literal")),
            Attribute::READONLY,
        )
        .property(
            js_string!("value"),
            JsValue::from(js_string!(s)),
            Attribute::READONLY,
        )
        .build();
    let _ = arr.push(JsValue::from(part), ctx);
    Ok(arr.into())
}

fn extract_date_ms(v: &JsValue, ctx: &mut Context) -> JsResult<f64> {
    // `Date` instances expose `getTime()`; numbers are already ms.
    if v.is_number() {
        return v.to_number(ctx);
    }
    if let Some(o) = v.as_object() {
        if let Ok(get_time) = o.get(js_string!("getTime"), ctx) {
            if let Some(fobj) = get_time.as_object() {
                if let Some(f) = boa_engine::object::builtins::JsFunction::from_object(fobj.clone())
                {
                    if let Ok(t) = f.call(v, &[], ctx) {
                        if let Ok(n) = t.to_number(ctx) {
                            return Ok(n);
                        }
                    }
                }
            }
        }
    }
    Ok(0.0)
}

fn format_date(ms: i64, opts: &DateTimeFormatOpts) -> String {
    // Decompose milliseconds since UNIX epoch (UTC) into civil
    // year/month/day/hour/min/sec via the standard algorithm.
    let (y, mo, d, h, mi, s) = ms_to_civil(ms);
    let mut parts: Vec<String> = Vec::new();

    if !opts.weekday.is_none() {
        parts.push(weekday_name(weekday_of(ms)));
    }

    if opts.year.is_some() || opts.month.is_some() || opts.day.is_some() {
        // Default: YYYY-MM-DD when all three are requested.
        let ystr = match opts.year.as_deref() {
            Some("2-digit") => format!("{:02}", y % 100),
            _ => format!("{y:04}"),
        };
        let mostr = match opts.month.as_deref() {
            Some("long") => long_month(mo),
            Some("short") => short_month(mo),
            Some("numeric") => format!("{mo}"),
            _ => format!("{mo:02}"),
        };
        let dstr = match opts.day.as_deref() {
            Some("numeric") => format!("{d}"),
            _ => format!("{d:02}"),
        };
        parts.push(format!("{mostr} {dstr}, {ystr}"));
    } else if opts.year.is_none() && opts.day.is_none() && opts.month.is_none() {
        parts.push(format!("{mo:02}/{d:02}/{y:04}"));
    }

    if opts.hour.is_some() || opts.minute.is_some() || opts.second.is_some() {
        let mut t = format!("{h:02}:{mi:02}");
        if opts.second.is_some() {
            t.push_str(&format!(":{s:02}"));
        }
        parts.push(t);
    }
    parts.join(", ")
}

fn ms_to_civil(ms: i64) -> (i32, u32, u32, u32, u32, u32) {
    // ms → seconds.
    let mut sec = ms.div_euclid(1000);
    let total_days = sec.div_euclid(86400);
    sec = sec.rem_euclid(86400);
    let hh = (sec / 3600) as u32;
    let mm = ((sec % 3600) / 60) as u32;
    let ss = (sec % 60) as u32;
    // Howard Hinnant's days-to-civil algorithm.
    let z = total_days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d, hh, mm, ss)
}

fn weekday_of(ms: i64) -> u32 {
    let days = ms.div_euclid(86_400_000);
    ((days + 4).rem_euclid(7)) as u32 // 1970-01-01 was Thursday (4).
}

fn long_month(m: u32) -> String {
    match m {
        1 => "January", 2 => "February", 3 => "March", 4 => "April",
        5 => "May", 6 => "June", 7 => "July", 8 => "August",
        9 => "September", 10 => "October", 11 => "November",
        12 => "December", _ => "",
    }
    .to_string()
}

fn short_month(m: u32) -> String {
    match m {
        1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr",
        5 => "May", 6 => "Jun", 7 => "Jul", 8 => "Aug",
        9 => "Sep", 10 => "Oct", 11 => "Nov", 12 => "Dec",
        _ => "",
    }
    .to_string()
}

fn weekday_name(w: u32) -> String {
    match w {
        0 => "Sunday", 1 => "Monday", 2 => "Tuesday", 3 => "Wednesday",
        4 => "Thursday", 5 => "Friday", 6 => "Saturday", _ => "",
    }
    .to_string()
}

// =================== Intl.Collator ===================

fn intl_collator_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(collator_compare), js_string!("compare"), 2)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn collator_compare(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = args.first().map(|v| v.to_string(ctx)).transpose()?;
    let b = args.get(1).map(|v| v.to_string(ctx)).transpose()?;
    let ord = match (a, b) {
        (Some(sa), Some(sb)) => sa
            .to_std_string_escaped()
            .cmp(&sb.to_std_string_escaped()) as i32,
        _ => 0,
    };
    Ok(JsValue::from(ord))
}

// =================== Intl.PluralRules ===================

fn intl_plural_rules_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(plural_select), js_string!("select"), 1)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn plural_select(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args.first().map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let cat = if (n - 1.0).abs() < f64::EPSILON { "one" } else { "other" };
    Ok(JsValue::from(js_string!(cat)))
}

// =================== Intl.ListFormat ===================

fn intl_list_format_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(list_format_fn), js_string!("format"), 1)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn list_format_fn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsValue::from(js_string!("")));
    };
    let Some(arr_obj) = arg.as_object() else {
        return Ok(arg.to_string(ctx)?.into());
    };
    let len = arr_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0) as usize;
    let mut items: Vec<String> = Vec::with_capacity(len);
    for i in 0..len {
        if let Ok(v) = arr_obj.get(i as u64, ctx) {
            if let Ok(s) = v.to_string(ctx) {
                items.push(s.to_std_string_escaped());
            }
        }
    }
    let out = match items.len() {
        0 => String::new(),
        1 => items.remove(0),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let last = items.pop().unwrap();
            format!("{}, and {}", items.join(", "), last)
        }
    };
    Ok(JsValue::from(js_string!(out)))
}

// =================== Intl.RelativeTimeFormat ===================

fn intl_relative_time_format_ctor(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(rtf_format), js_string!("format"), 2)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn rtf_format(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.first().map(|x| x.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let unit = args
        .get(1)
        .map(|x| x.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "second".to_string());
    let n = v.round() as i64;
    let unit = unit.trim_end_matches('s');
    let s = if n == 0 {
        format!("now")
    } else if n < 0 {
        format!("{} {}{} ago", -n, unit, if -n == 1 { "" } else { "s" })
    } else {
        format!("in {} {}{}", n, unit, if n == 1 { "" } else { "s" })
    };
    Ok(JsValue::from(js_string!(s)))
}

// =================== Intl.DisplayNames ===================

fn intl_display_names_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(display_names_of), js_string!("of"), 1)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn display_names_of(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(code)))
}

// =================== Intl.Locale ===================

fn intl_locale_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let tag = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "en".to_string());
    let (lang, region) = split_tag(&tag);
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .property(
                js_string!("baseName"),
                JsValue::from(js_string!(tag.clone())),
                Attribute::READONLY,
            )
            .property(
                js_string!("language"),
                JsValue::from(js_string!(lang)),
                Attribute::READONLY,
            )
            .property(
                js_string!("region"),
                JsValue::from(js_string!(region)),
                Attribute::READONLY,
            )
            .build(),
    ))
}

fn split_tag(tag: &str) -> (String, String) {
    let mut parts = tag.split('-');
    let lang = parts.next().unwrap_or("").to_string();
    let region = parts
        .find(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or("")
        .to_string();
    (lang, region)
}

// =================== Intl.Segmenter ===================

fn intl_segmenter_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(segmenter_segment), js_string!("segment"), 1)
            .function(
                NativeFunction::from_fn_ptr(resolved_options_stub),
                js_string!("resolvedOptions"),
                0,
            )
            .build(),
    ))
}

fn segmenter_segment(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|v| v.to_string(ctx))
        .transpose()?
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    // Spec returns a Segments iterable; we return an Array of
    // { segment, index } objects (which is what most pages iterate
    // over anyway with for..of).
    let arr = JsArray::new(ctx);
    let mut idx = 0;
    for word in text.split_whitespace() {
        let segment = ObjectInitializer::new(ctx)
            .property(
                js_string!("segment"),
                JsValue::from(js_string!(word)),
                Attribute::READONLY,
            )
            .property(
                js_string!("index"),
                JsValue::from(idx as u32),
                Attribute::READONLY,
            )
            .build();
        let _ = arr.push(JsValue::from(segment), ctx);
        idx += word.len() + 1;
    }
    Ok(arr.into())
}
