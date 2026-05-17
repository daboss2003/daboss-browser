//! `localStorage` / `sessionStorage` bindings.
//!
//! Both are exposed as JS objects with the spec method API:
//! `length`, `key(i)`, `getItem(k)`, `setItem(k, v)`, `removeItem(k)`,
//! `clear()`. Spec-style direct property access (`localStorage.foo = 1`)
//! is **not** supported — that requires a `Proxy` and we haven't wired
//! one in for storage yet. Real-world code that uses the method API
//! works; code that does `localStorage.foo = ...` silently no-ops.
//!
//! Persistence model (toy):
//!  * `sessionStorage` lives on the [`super::JsEngine`] and is recreated
//!    each navigation, so it survives across event handler ticks /
//!    timers but resets when the user goes to a new URL.
//!  * `localStorage` lives on the [`crate::Browser`] (as
//!    `Rc<RefCell<StorageArea>>`) so it persists across navigations
//!    within a single browser run. There's no disk persistence — the
//!    map dies with the process. Adding that lands in a later phase.
//!
//! Storage areas are *unscoped* in this toy: all pages see the same
//! `localStorage`. Real browsers scope by origin (scheme + host + port).
//! Wiring origin scoping is straightforward once tabs land.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsResult, JsValue,
    NativeFunction,
};

pub type StorageArea = Rc<RefCell<HashMap<String, String>>>;

thread_local! {
    /// Active `localStorage` map during script execution. Installed by
    /// the [`super::JsEngine`] thread-local plumbing.
    pub(crate) static JS_LOCAL_STORAGE: RefCell<Option<StorageArea>> =
        const { RefCell::new(None) };

    /// Active `sessionStorage` map (per page).
    pub(crate) static JS_SESSION_STORAGE: RefCell<Option<StorageArea>> =
        const { RefCell::new(None) };
}

#[derive(Copy, Clone)]
enum Which {
    Local,
    Session,
}

const WHICH_KEY: &str = "__storage_kind";

/// Install `localStorage` and `sessionStorage` on the global object.
pub fn install(ctx: &mut Context) {
    let local = build_storage(ctx, Which::Local);
    let session = build_storage(ctx, Which::Session);
    let _ = ctx.register_global_property(js_string!("localStorage"), local, Attribute::all());
    let _ = ctx.register_global_property(js_string!("sessionStorage"), session, Attribute::all());
}

fn build_storage(ctx: &mut Context, which: Which) -> boa_engine::JsObject {
    let realm = ctx.realm().clone();
    let length_get = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(length_getter),
    )
    .build();

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(WHICH_KEY),
        JsValue::from(which as u32),
        Attribute::READONLY,
    );
    init.accessor(
        js_string!("length"),
        Some(length_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.function(
        NativeFunction::from_fn_ptr(get_item),
        js_string!("getItem"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(set_item),
        js_string!("setItem"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(remove_item),
        js_string!("removeItem"),
        1,
    );
    init.function(NativeFunction::from_fn_ptr(clear), js_string!("clear"), 0);
    init.function(NativeFunction::from_fn_ptr(key), js_string!("key"), 1);
    init.build()
}

fn area_for_this(this: &JsValue, ctx: &mut Context) -> Option<StorageArea> {
    let obj = this.as_object()?;
    let kind = obj.get(js_string!(WHICH_KEY), ctx).ok()?.to_u32(ctx).ok()?;
    let which = if kind == Which::Local as u32 {
        Which::Local
    } else {
        Which::Session
    };
    let slot = match which {
        Which::Local => &JS_LOCAL_STORAGE,
        Which::Session => &JS_SESSION_STORAGE,
    };
    slot.with(|s| s.borrow().as_ref().cloned())
}

fn length_getter(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::from(0_u32));
    };
    let len = area.borrow().len() as u32;
    Ok(JsValue::from(len))
}

fn get_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(k) = args.first() else {
        return Ok(JsValue::null());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = area.borrow().get(&key).cloned();
    Ok(match value {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn set_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = v.to_string(ctx)?.to_std_string_escaped();
    area.borrow_mut().insert(key, value);
    Ok(JsValue::undefined())
}

fn remove_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(k) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    area.borrow_mut().remove(&key);
    Ok(JsValue::undefined())
}

fn clear(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    area.borrow_mut().clear();
    Ok(JsValue::undefined())
}

fn key(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(area) = area_for_this(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(idx_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let Ok(idx) = idx_val.to_u32(ctx) else {
        return Ok(JsValue::null());
    };
    let entry = {
        let area = area.borrow();
        let mut keys: Vec<String> = area.keys().cloned().collect();
        keys.sort();
        keys.into_iter().nth(idx as usize)
    };
    Ok(match entry {
        Some(k) => JsValue::from(js_string!(k)),
        None => JsValue::null(),
    })
}
