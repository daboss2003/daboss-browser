//! `SharedArrayBuffer` + `Atomics.*` shim.
//!
//! True shared memory + atomic ops require cross-thread access; our
//! JS runs single-threaded per page, so we don't need real
//! synchronization. We expose the surface so pages that gate
//! features on `typeof Atomics !== "undefined"` work, and the ops
//! return correct values when called on regular TypedArrays.
//!
//! `SharedArrayBuffer` is implemented as an alias of `ArrayBuffer` —
//! pages can `new SharedArrayBuffer(n)` and slice typed-array views
//! over it; the memory just doesn't cross threads. This is wrong
//! per spec but doesn't crash and matches how single-thread test
//! harnesses use SAB. `Atomics.wait` short-circuits to "not-equal"
//! when the index doesn't match (so `wait` never actually parks the
//! single thread).

use boa_engine::{
    js_string,
    object::{builtins::JsArrayBuffer, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

pub fn install(ctx: &mut Context) {
    install_sab(ctx);
    install_atomics(ctx);
    install_cross_origin_isolated(ctx);
}

fn install_sab(ctx: &mut Context) {
    let _ = ctx.register_global_callable(
        js_string!("SharedArrayBuffer"),
        1,
        NativeFunction::from_fn_ptr(sab_ctor),
    );
}

fn sab_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let len = args
        .first()
        .map(|v| v.to_u32(ctx))
        .transpose()?
        .unwrap_or(0) as usize;
    match JsArrayBuffer::from_byte_block(vec![0u8; len], ctx) {
        Ok(ab) => Ok(JsValue::from(ab)),
        Err(_) => Ok(JsValue::null()),
    }
}

fn install_atomics(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let atomics = ObjectInitializer::new(ctx)
        .property(js_string!("load"), JsValue::from(mk(atomics_load)), Attribute::READONLY)
        .property(js_string!("store"), JsValue::from(mk(atomics_store)), Attribute::READONLY)
        .property(js_string!("add"), JsValue::from(mk(atomics_add)), Attribute::READONLY)
        .property(js_string!("sub"), JsValue::from(mk(atomics_sub)), Attribute::READONLY)
        .property(js_string!("and"), JsValue::from(mk(atomics_and)), Attribute::READONLY)
        .property(js_string!("or"), JsValue::from(mk(atomics_or)), Attribute::READONLY)
        .property(js_string!("xor"), JsValue::from(mk(atomics_xor)), Attribute::READONLY)
        .property(
            js_string!("exchange"),
            JsValue::from(mk(atomics_exchange)),
            Attribute::READONLY,
        )
        .property(
            js_string!("compareExchange"),
            JsValue::from(mk(atomics_compare_exchange)),
            Attribute::READONLY,
        )
        .property(js_string!("wait"), JsValue::from(mk(atomics_wait)), Attribute::READONLY)
        .property(js_string!("notify"), JsValue::from(mk(atomics_notify)), Attribute::READONLY)
        .property(
            js_string!("isLockFree"),
            JsValue::from(mk(atomics_is_lock_free)),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("Atomics"),
        atomics,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn install_cross_origin_isolated(ctx: &mut Context) {
    // We don't honour COOP/COEP, so isolation is always false. Set
    // both the global and `window.crossOriginIsolated`.
    let _ = ctx.register_global_property(
        js_string!("crossOriginIsolated"),
        JsValue::from(false),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

// ============== helpers ==============

fn idx_of(v: &JsValue, ctx: &mut Context) -> JsResult<i64> {
    v.to_number(ctx).map(|n| n as i64)
}

fn read_int(view: &JsValue, idx: i64, ctx: &mut Context) -> JsResult<f64> {
    let Some(obj) = view.as_object() else {
        return Ok(0.0);
    };
    obj.get(idx as u64, ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .map(Ok)
        .unwrap_or(Ok(0.0))
}

fn write_int(view: &JsValue, idx: i64, value: f64, ctx: &mut Context) -> JsResult<()> {
    if let Some(obj) = view.as_object() {
        let _ = obj.set(idx as u64, JsValue::from(value), false, ctx);
    }
    Ok(())
}

fn atomics_load(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let view = args.first().cloned().unwrap_or(JsValue::undefined());
    let idx = idx_of(args.get(1).unwrap_or(&JsValue::from(0)), ctx)?;
    let v = read_int(&view, idx, ctx)?;
    Ok(JsValue::from(v))
}

fn atomics_store(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let view = args.first().cloned().unwrap_or(JsValue::undefined());
    let idx = idx_of(args.get(1).unwrap_or(&JsValue::from(0)), ctx)?;
    let val = args.get(2).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    write_int(&view, idx, val, ctx)?;
    Ok(JsValue::from(val))
}

fn read_modify_write(
    args: &[JsValue],
    ctx: &mut Context,
    op: impl FnOnce(f64, f64) -> f64,
) -> JsResult<JsValue> {
    let view = args.first().cloned().unwrap_or(JsValue::undefined());
    let idx = idx_of(args.get(1).unwrap_or(&JsValue::from(0)), ctx)?;
    let arg = args.get(2).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let prev = read_int(&view, idx, ctx)?;
    let new = op(prev, arg);
    write_int(&view, idx, new, ctx)?;
    Ok(JsValue::from(prev))
}

fn atomics_add(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |a, b| a + b)
}

fn atomics_sub(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |a, b| a - b)
}

fn atomics_and(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |a, b| ((a as i64) & (b as i64)) as f64)
}

fn atomics_or(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |a, b| ((a as i64) | (b as i64)) as f64)
}

fn atomics_xor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |a, b| ((a as i64) ^ (b as i64)) as f64)
}

fn atomics_exchange(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    read_modify_write(args, ctx, |_, b| b)
}

fn atomics_compare_exchange(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let view = args.first().cloned().unwrap_or(JsValue::undefined());
    let idx = idx_of(args.get(1).unwrap_or(&JsValue::from(0)), ctx)?;
    let expected = args.get(2).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let replacement = args.get(3).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let prev = read_int(&view, idx, ctx)?;
    if (prev - expected).abs() < f64::EPSILON {
        write_int(&view, idx, replacement, ctx)?;
    }
    Ok(JsValue::from(prev))
}

fn atomics_wait(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Single-threaded — never actually wait. Return "not-equal" if
    // the stored value already differs (the common SAB-mutex
    // fast-path skips wait()), else "ok" instantly.
    let view = args.first().cloned().unwrap_or(JsValue::undefined());
    let idx = idx_of(args.get(1).unwrap_or(&JsValue::from(0)), ctx)?;
    let value = args.get(2).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    let actual = read_int(&view, idx, ctx)?;
    let verdict = if (actual - value).abs() > f64::EPSILON {
        "not-equal"
    } else {
        "ok"
    };
    Ok(JsValue::from(js_string!(verdict)))
}

fn atomics_notify(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // No one to notify on a single-threaded runtime.
    Ok(JsValue::from(0u32))
}

fn atomics_is_lock_free(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let size = args.first().map(|v| v.to_u32(ctx)).transpose()?.unwrap_or(4);
    let lock_free = matches!(size, 1 | 2 | 4 | 8);
    Ok(JsValue::from(lock_free))
}
