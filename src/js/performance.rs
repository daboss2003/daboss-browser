//! Performance API + `PerformanceObserver`.
//!
//! Replaces the bare `performance.now()` install with the surface
//! observability tooling probes:
//!   * `performance.now()` — milliseconds since the engine's origin.
//!   * `performance.timeOrigin` — wall-clock anchor for the origin.
//!   * `performance.mark(name)` / `performance.measure(name, start?,
//!     end?)`.
//!   * `performance.clearMarks(name?)` / `performance.clearMeasures(name?)`.
//!   * `performance.getEntries()` /
//!     `performance.getEntriesByType(type)` /
//!     `performance.getEntriesByName(name, type?)`.
//!   * `new PerformanceObserver(cb).observe({type, buffered})` —
//!     callback fires once per `record_*` call (and synchronously
//!     with buffered entries when `buffered: true`).
//!
//! Real spec exposes a much larger taxonomy of entry types
//! (navigation, resource, paint, longtask, largest-contentful-paint,
//! event, first-input, layout-shift). For the toy we only mint
//! "mark" / "measure" entries from JS. The browser shell could
//! synthesise the others later — record_entry below is public so
//! the rest of the crate can push them.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::SystemTime;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArray, JsFunction},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

#[derive(Clone, Debug)]
pub struct PerformanceEntry {
    pub name: String,
    pub entry_type: String,
    pub start_time_ms: f64,
    pub duration_ms: f64,
}

#[derive(Clone)]
struct ObserverEntry {
    callback: JsFunction,
    types: Vec<String>,
}

thread_local! {
    pub(crate) static PERFORMANCE_ENTRIES: RefCell<Vec<PerformanceEntry>> =
        const { RefCell::new(Vec::new()) };
    pub(crate) static OBSERVERS: RefCell<Vec<Rc<ObserverEntry>>> =
        const { RefCell::new(Vec::new()) };
    pub(crate) static PERF_ORIGIN: std::time::Instant = std::time::Instant::now();
    /// Wall-clock anchor — set once when the engine boots. JS reads
    /// it via `performance.timeOrigin`.
    pub(crate) static TIME_ORIGIN_MS: f64 = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    /// Pending observer callback invocations. Drained alongside
    /// microtasks so they fire with a live JS context.
    pub(crate) static OBSERVER_QUEUE: RefCell<Vec<DeferredObserve>> =
        const { RefCell::new(Vec::new()) };
}

pub struct DeferredObserve {
    pub callback: JsFunction,
    pub entries: Vec<PerformanceEntry>,
}

fn now_ms() -> f64 {
    PERF_ORIGIN.with(|t| t.elapsed().as_secs_f64() * 1000.0)
}

pub fn install(ctx: &mut Context) {
    install_performance(ctx);
    install_performance_observer(ctx);
}

fn install_performance(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let methods: &[(&str, NativeFunction, usize)] = &[
        ("now", NativeFunction::from_fn_ptr(performance_now), 0),
        ("mark", NativeFunction::from_fn_ptr(performance_mark), 2),
        ("measure", NativeFunction::from_fn_ptr(performance_measure), 4),
        ("clearMarks", NativeFunction::from_fn_ptr(performance_clear_marks), 1),
        ("clearMeasures", NativeFunction::from_fn_ptr(performance_clear_measures), 1),
        ("clearResourceTimings", NativeFunction::from_fn_ptr(noop), 0),
        ("getEntries", NativeFunction::from_fn_ptr(performance_get_entries), 0),
        ("getEntriesByType", NativeFunction::from_fn_ptr(performance_get_by_type), 1),
        ("getEntriesByName", NativeFunction::from_fn_ptr(performance_get_by_name), 2),
        ("toJSON", NativeFunction::from_fn_ptr(performance_to_json), 0),
    ];
    let mut entries: Vec<(&str, JsValue)> = Vec::new();
    for (name, f, _) in methods {
        let func = boa_engine::object::FunctionObjectBuilder::new(&realm, f.clone()).build();
        entries.push((name, JsValue::from(func)));
    }
    let mut b = ObjectInitializer::new(ctx);
    for (name, val) in entries {
        b.property(js_string!(name), val, Attribute::READONLY);
    }
    let time_origin = TIME_ORIGIN_MS.with(|t| *t);
    b.property(
        js_string!("timeOrigin"),
        JsValue::from(time_origin),
        Attribute::READONLY,
    );
    let perf = b.build();
    let _ = ctx.register_global_property(
        js_string!("performance"),
        perf,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn performance_now(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(now_ms()))
}

fn performance_mark(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // Optional `{ startTime, detail }` argument — we honour startTime.
    let start_time = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("startTime"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or_else(now_ms);
    let entry = PerformanceEntry {
        name: name.clone(),
        entry_type: "mark".to_string(),
        start_time_ms: start_time,
        duration_ms: 0.0,
    };
    record_entry(entry.clone());
    Ok(build_entry_object(ctx, &entry))
}

fn performance_measure(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // Second arg is either a string (mark name) or an object with
    // `{ start, end, duration }`.
    let (start_ms, end_ms) = resolve_measure_bounds(args.get(1), args.get(2), ctx);
    let duration = (end_ms - start_ms).max(0.0);
    let entry = PerformanceEntry {
        name: name.clone(),
        entry_type: "measure".to_string(),
        start_time_ms: start_ms,
        duration_ms: duration,
    };
    record_entry(entry.clone());
    Ok(build_entry_object(ctx, &entry))
}

fn resolve_measure_bounds(
    second: Option<&JsValue>,
    third: Option<&JsValue>,
    ctx: &mut Context,
) -> (f64, f64) {
    let lookup_mark = |s: &str| -> Option<f64> {
        PERFORMANCE_ENTRIES.with(|r| {
            r.borrow()
                .iter()
                .rev()
                .find(|e| e.entry_type == "mark" && e.name == s)
                .map(|e| e.start_time_ms)
        })
    };
    // Second arg can be an options object `{ start, end, duration }`.
    if let Some(opts) = second.and_then(|v| v.as_object()) {
        let start_opt = opts
            .get(js_string!("start"), ctx)
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_null());
        let end_opt = opts
            .get(js_string!("end"), ctx)
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_null());
        let duration_opt = opts
            .get(js_string!("duration"), ctx)
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_null());
        let resolve_endpoint = |v: &JsValue, ctx: &mut Context| -> Option<f64> {
            if v.is_string() {
                let s = v.to_string(ctx).ok()?.to_std_string_escaped();
                lookup_mark(&s)
            } else {
                v.to_number(ctx).ok()
            }
        };
        let start = start_opt.and_then(|v| resolve_endpoint(&v, ctx));
        let end = end_opt.and_then(|v| resolve_endpoint(&v, ctx));
        let duration = duration_opt.and_then(|v| v.to_number(ctx).ok());
        return match (start, end, duration) {
            (Some(s), Some(e), _) => (s, e),
            (Some(s), None, Some(d)) => (s, s + d),
            (None, Some(e), Some(d)) => (e - d, e),
            (Some(s), None, None) => (s, now_ms()),
            (None, Some(e), None) => (0.0, e),
            (None, None, _) => (0.0, now_ms()),
        };
    }
    // String mark names form.
    let start = second
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .and_then(|s| lookup_mark(&s));
    let end = third
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .and_then(|s| lookup_mark(&s));
    (start.unwrap_or(0.0), end.unwrap_or_else(now_ms))
}

fn performance_clear_marks(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    PERFORMANCE_ENTRIES.with(|r| {
        r.borrow_mut().retain(|e| {
            if e.entry_type != "mark" {
                return true;
            }
            match &name {
                Some(n) => &e.name != n,
                None => false,
            }
        });
    });
    Ok(JsValue::undefined())
}

fn performance_clear_measures(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let name = args
        .first()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    PERFORMANCE_ENTRIES.with(|r| {
        r.borrow_mut().retain(|e| {
            if e.entry_type != "measure" {
                return true;
            }
            match &name {
                Some(n) => &e.name != n,
                None => false,
            }
        });
    });
    Ok(JsValue::undefined())
}

fn performance_get_entries(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let entries: Vec<PerformanceEntry> = PERFORMANCE_ENTRIES.with(|r| r.borrow().clone());
    let arr = JsArray::new(ctx);
    for e in entries {
        let _ = arr.push(build_entry_object(ctx, &e), ctx);
    }
    Ok(arr.into())
}

fn performance_get_by_type(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ty = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let entries: Vec<PerformanceEntry> = PERFORMANCE_ENTRIES.with(|r| {
        r.borrow()
            .iter()
            .filter(|e| e.entry_type == ty)
            .cloned()
            .collect()
    });
    let arr = JsArray::new(ctx);
    for e in entries {
        let _ = arr.push(build_entry_object(ctx, &e), ctx);
    }
    Ok(arr.into())
}

fn performance_get_by_name(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let ty = args
        .get(1)
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    let entries: Vec<PerformanceEntry> = PERFORMANCE_ENTRIES.with(|r| {
        r.borrow()
            .iter()
            .filter(|e| e.name == name && ty.as_ref().map_or(true, |t| &e.entry_type == t))
            .cloned()
            .collect()
    });
    let arr = JsArray::new(ctx);
    for e in entries {
        let _ = arr.push(build_entry_object(ctx, &e), ctx);
    }
    Ok(arr.into())
}

fn performance_to_json(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let time_origin = TIME_ORIGIN_MS.with(|t| *t);
    Ok(ObjectInitializer::new(ctx)
        .property(
            js_string!("timeOrigin"),
            JsValue::from(time_origin),
            Attribute::READONLY,
        )
        .build()
        .into())
}

fn build_entry_object(ctx: &mut Context, entry: &PerformanceEntry) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(
            js_string!("name"),
            JsValue::from(js_string!(entry.name.clone())),
            Attribute::READONLY,
        )
        .property(
            js_string!("entryType"),
            JsValue::from(js_string!(entry.entry_type.clone())),
            Attribute::READONLY,
        )
        .property(
            js_string!("startTime"),
            JsValue::from(entry.start_time_ms),
            Attribute::READONLY,
        )
        .property(
            js_string!("duration"),
            JsValue::from(entry.duration_ms),
            Attribute::READONLY,
        )
        .build()
        .into()
}

/// Record an entry into the registry and notify any matching
/// observers. Public so the rest of the crate can push entries of
/// other types (paint, resource, longtask) as the browser shell
/// generates them.
pub fn record_entry(entry: PerformanceEntry) {
    PERFORMANCE_ENTRIES.with(|r| r.borrow_mut().push(entry.clone()));
    let matching: Vec<JsFunction> = OBSERVERS.with(|r| {
        r.borrow()
            .iter()
            .filter(|o| o.types.contains(&entry.entry_type))
            .map(|o| o.callback.clone())
            .collect()
    });
    for cb in matching {
        OBSERVER_QUEUE.with(|q| {
            q.borrow_mut().push(DeferredObserve {
                callback: cb,
                entries: vec![entry.clone()],
            });
        });
    }
}

// ============ PerformanceObserver ============

fn install_performance_observer(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("PerformanceObserver"),
        1,
        NativeFunction::from_fn_ptr(observer_ctor),
    )
    .ok();
}

const OBSERVER_REF_KEY: &str = "__perf_observer_idx";

fn observer_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(cb_val) = args.first() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("PerformanceObserver: requires a callback")
            .into());
    };
    let Some(cb_obj) = cb_val.as_object() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("PerformanceObserver: callback must be callable")
            .into());
    };
    let Some(callback) = JsFunction::from_object(cb_obj.clone()) else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("PerformanceObserver: callback must be a function")
            .into());
    };
    let entry = Rc::new(ObserverEntry {
        callback,
        types: Vec::new(),
    });
    let idx = OBSERVERS.with(|r| {
        let mut v = r.borrow_mut();
        v.push(entry);
        v.len() - 1
    });
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!(OBSERVER_REF_KEY),
            JsValue::from(idx as u32),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(observer_observe),
            js_string!("observe"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(observer_disconnect),
            js_string!("disconnect"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(observer_take_records),
            js_string!("takeRecords"),
            0,
        )
        .build();
    Ok(JsValue::from(obj))
}

fn observer_idx_of(this: &JsValue, ctx: &mut Context) -> Option<usize> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(OBSERVER_REF_KEY), ctx).ok()?;
    Some(v.to_u32(ctx).ok()? as usize)
}

fn observer_observe(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx) = observer_idx_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(opts) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    // Support both `{ type: "mark" }` and `{ entryTypes: ["mark", "measure"] }`.
    let mut types: Vec<String> = Vec::new();
    if let Ok(t) = opts.get(js_string!("type"), ctx) {
        if !t.is_undefined() && !t.is_null() {
            if let Ok(s) = t.to_string(ctx) {
                types.push(s.to_std_string_escaped());
            }
        }
    }
    if let Ok(et) = opts.get(js_string!("entryTypes"), ctx) {
        if let Some(arr) = et.as_object() {
            let len = arr
                .get(js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .unwrap_or(0);
            for i in 0..len {
                if let Ok(v) = arr.get(i, ctx) {
                    if let Ok(s) = v.to_string(ctx) {
                        types.push(s.to_std_string_escaped());
                    }
                }
            }
        }
    }
    let buffered = opts
        .get(js_string!("buffered"), ctx)
        .ok()
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    let callback = OBSERVERS.with(|r| -> Option<JsFunction> {
        let mut v = r.borrow_mut();
        let existing = v.get(idx).cloned()?;
        let cb = existing.callback.clone();
        let mut existing = (*existing).clone();
        // Append types (multiple `observe` calls extend the list).
        for t in &types {
            if !existing.types.contains(t) {
                existing.types.push(t.clone());
            }
        }
        v[idx] = Rc::new(existing);
        Some(cb)
    });
    if buffered {
        if let Some(callback) = callback {
            let buffered_entries: Vec<PerformanceEntry> = PERFORMANCE_ENTRIES.with(|r| {
                r.borrow()
                    .iter()
                    .filter(|e| types.contains(&e.entry_type))
                    .cloned()
                    .collect()
            });
            if !buffered_entries.is_empty() {
                OBSERVER_QUEUE.with(|q| {
                    q.borrow_mut().push(DeferredObserve {
                        callback,
                        entries: buffered_entries,
                    });
                });
            }
        }
    }
    Ok(JsValue::undefined())
}

fn observer_disconnect(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(idx) = observer_idx_of(this, ctx) {
        OBSERVERS.with(|r| {
            let mut v = r.borrow_mut();
            if idx < v.len() {
                v[idx] = Rc::new(ObserverEntry {
                    callback: v[idx].callback.clone(),
                    types: Vec::new(),
                });
            }
        });
    }
    Ok(JsValue::undefined())
}

fn observer_take_records(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // We resolve observers synchronously through OBSERVER_QUEUE, so
    // takeRecords() returns an empty list — there's nothing buffered
    // waiting for the next microtask.
    Ok(JsArray::new(ctx).into())
}

/// Drain pending observer callbacks. Called alongside microtask
/// draining so the JS context is live.
pub fn drain_observers(ctx: &mut Context) {
    let pending: Vec<DeferredObserve> =
        OBSERVER_QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for d in pending {
        // Build a PerformanceObserverEntryList shim with
        // getEntries / getEntriesByType / getEntriesByName.
        let entries = d.entries.clone();
        let arr = JsArray::new(ctx);
        for e in &entries {
            let _ = arr.push(build_entry_object(ctx, e), ctx);
        }
        let realm = ctx.realm().clone();
        let get_entries_fn = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(noop_entries),
        )
        .build();
        let list_obj = ObjectInitializer::new(ctx)
            .property(
                js_string!("__entries"),
                JsValue::from(arr.clone()),
                Attribute::READONLY,
            )
            .property(
                js_string!("getEntries"),
                JsValue::from(get_entries_fn.clone()),
                Attribute::READONLY,
            )
            .function(
                NativeFunction::from_fn_ptr(list_get_entries),
                js_string!("getEntries"),
                0,
            )
            .function(
                NativeFunction::from_fn_ptr(list_get_by_type),
                js_string!("getEntriesByType"),
                1,
            )
            .function(
                NativeFunction::from_fn_ptr(list_get_by_name),
                js_string!("getEntriesByName"),
                2,
            )
            .build();
        let _ = d.callback.call(
            &JsValue::undefined(),
            &[JsValue::from(list_obj), JsValue::undefined()],
            ctx,
        );
    }
}

fn noop_entries(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsArray::new(ctx).into())
}

fn list_get_entries(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsArray::new(ctx).into());
    };
    obj.get(js_string!("__entries"), ctx)
}

fn list_get_by_type(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsArray::new(ctx).into());
    };
    let entries_val = obj.get(js_string!("__entries"), ctx)?;
    let Some(arr) = entries_val.as_object() else {
        return Ok(JsArray::new(ctx).into());
    };
    let ty = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let out = JsArray::new(ctx);
    for i in 0..len {
        let Ok(item) = arr.get(i, ctx) else { continue };
        let entry_type = item
            .as_object()
            .and_then(|o| o.get(js_string!("entryType"), ctx).ok())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        if entry_type == ty {
            let _ = out.push(item, ctx);
        }
    }
    Ok(out.into())
}

fn list_get_by_name(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsArray::new(ctx).into());
    };
    let entries_val = obj.get(js_string!("__entries"), ctx)?;
    let Some(arr) = entries_val.as_object() else {
        return Ok(JsArray::new(ctx).into());
    };
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let out = JsArray::new(ctx);
    for i in 0..len {
        let Ok(item) = arr.get(i, ctx) else { continue };
        let entry_name = item
            .as_object()
            .and_then(|o| o.get(js_string!("name"), ctx).ok())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        if entry_name == name {
            let _ = out.push(item, ctx);
        }
    }
    Ok(out.into())
}
