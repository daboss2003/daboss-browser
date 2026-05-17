//! IndexedDB (toy).
//!
//! In-memory key/value backed by `HashMap`, scoped per origin (same
//! pattern as `localStorage`). The async event model is preserved at
//! the JS API level — `open()`, `get()`, `put()` etc. all return
//! request objects whose `onsuccess` handlers fire on the same tick.
//! No version migration callbacks, no transactions modelled
//! independently of the store, no cursors.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer,
    property::Attribute, Context, JsResult, JsValue, NativeFunction,
};

pub type IdbStore = HashMap<String, String>;
pub type IdbDatabase = HashMap<String, IdbStore>;
pub type IdbState = Rc<RefCell<HashMap<String, IdbDatabase>>>;

thread_local! {
    pub(crate) static JS_IDB: RefCell<Option<IdbState>> = const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };
    let _ = getter;
    let global = ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(idb_open),
            js_string!("open"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(idb_delete_database),
            js_string!("deleteDatabase"),
            1,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("indexedDB"),
        global,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn idb_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    // Ensure the database exists.
    JS_IDB.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state.borrow_mut().entry(name.clone()).or_default();
        }
    });
    let db_obj = make_database(ctx, &name);
    // Build a request object whose onsuccess fires synchronously.
    let request = make_request(ctx, db_obj);
    Ok(JsValue::from(request))
}

fn idb_delete_database(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(name_val) = args.first() {
        let name = name_val.to_string(ctx)?.to_std_string_escaped();
        JS_IDB.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.borrow_mut().remove(&name);
            }
        });
    }
    let request = make_request(ctx, JsValue::undefined());
    Ok(JsValue::from(request))
}

/// Build a `request`-shaped object with `result`, `error`, and an
/// `onsuccess` accessor that fires synchronously when first assigned
/// (matches the common pattern of `req.onsuccess = e => ...`).
fn make_request(ctx: &mut Context, result: JsValue) -> boa_engine::JsObject {
    let realm = ctx.realm().clone();
    let on_success_set = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(request_set_onsuccess),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!("result"), result.clone(), Attribute::all());
    b.property(js_string!("error"), JsValue::null(), Attribute::all());
    // The synthesised target stays a property; we read it in the
    // setter so the handler sees `event.target.result`.
    b.accessor(
        js_string!("onsuccess"),
        None,
        Some(on_success_set),
        Attribute::ENUMERABLE,
    );
    b.property(js_string!("onerror"), JsValue::null(), Attribute::all());
    b.build()
}

/// Fire the assigned `onsuccess` callback immediately. We use an
/// accessor's setter for this so JS code that writes
/// `req.onsuccess = fn` triggers without us having to drain a queue.
fn request_set_onsuccess(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(fn_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(obj) = fn_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(cb) = JsFunction::from_object(obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    let result = this
        .as_object()
        .and_then(|o| o.get(js_string!("result"), ctx).ok())
        .unwrap_or(JsValue::undefined());
    let event_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("target"),
            JsValue::from(
                ObjectInitializer::new(ctx)
                    .property(js_string!("result"), result, Attribute::READONLY)
                    .build(),
            ),
            Attribute::READONLY,
        )
        .build();
    let _ = cb.call(&JsValue::undefined(), &[JsValue::from(event_obj)], ctx);
    Ok(JsValue::undefined())
}

fn make_database(ctx: &mut Context, name: &str) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("name"),
        JsValue::from(js_string!(name.to_string())),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(db_create_object_store),
        js_string!("createObjectStore"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(db_transaction),
        js_string!("transaction"),
        2,
    );
    b.property(js_string!("__db_name"), JsValue::from(js_string!(name.to_string())), Attribute::READONLY);
    JsValue::from(b.build())
}

fn db_create_object_store(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let store_name = name_val.to_string(ctx)?.to_std_string_escaped();
    let db_name = this
        .as_object()
        .and_then(|o| o.get(js_string!("__db_name"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    JS_IDB.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(db_name)
                .or_default()
                .entry(store_name.clone())
                .or_default();
        }
    });
    Ok(make_object_store(ctx, &store_name))
}

fn db_transaction(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // We don't model transactions distinctly — `transaction(stores,
    // mode).objectStore(name)` collapses to the same store the
    // database itself holds.
    let db_name = this
        .as_object()
        .and_then(|o| o.get(js_string!("__db_name"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let _ = args;
    let tx = ObjectInitializer::new(ctx)
        .property(
            js_string!("__db_name"),
            JsValue::from(js_string!(db_name)),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(tx_object_store),
            js_string!("objectStore"),
            1,
        )
        .property(
            js_string!("oncomplete"),
            JsValue::null(),
            Attribute::all(),
        )
        .build();
    Ok(JsValue::from(tx))
}

fn tx_object_store(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let store_name = name_val.to_string(ctx)?.to_std_string_escaped();
    let db_name = this
        .as_object()
        .and_then(|o| o.get(js_string!("__db_name"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    // Ensure the store exists.
    JS_IDB.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(db_name.clone())
                .or_default()
                .entry(store_name.clone())
                .or_default();
        }
    });
    let store = ObjectInitializer::new(ctx)
        .property(
            js_string!("__db_name"),
            JsValue::from(js_string!(db_name)),
            Attribute::READONLY,
        )
        .property(
            js_string!("__store_name"),
            JsValue::from(js_string!(store_name.clone())),
            Attribute::READONLY,
        )
        .function(NativeFunction::from_fn_ptr(store_put), js_string!("put"), 2)
        .function(NativeFunction::from_fn_ptr(store_get), js_string!("get"), 1)
        .function(NativeFunction::from_fn_ptr(store_delete), js_string!("delete"), 1)
        .function(NativeFunction::from_fn_ptr(store_clear), js_string!("clear"), 0)
        .function(NativeFunction::from_fn_ptr(store_get_all_keys), js_string!("getAllKeys"), 0)
        .build();
    Ok(JsValue::from(store))
}

fn make_object_store(ctx: &mut Context, store_name: &str) -> JsValue {
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("name"),
            JsValue::from(js_string!(store_name.to_string())),
            Attribute::READONLY,
        )
        .build();
    JsValue::from(obj)
}

fn store_keys(this: &JsValue, ctx: &mut Context) -> (String, String) {
    let obj = match this.as_object() {
        Some(o) => o,
        None => return (String::new(), String::new()),
    };
    let db = obj
        .get(js_string!("__db_name"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let store = obj
        .get(js_string!("__store_name"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    (db, store)
}

fn store_put(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    let (Some(value), Some(key_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::from(make_request(ctx, JsValue::undefined())));
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let value_str = value.to_string(ctx)?.to_std_string_escaped();
    JS_IDB.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(db_name)
                .or_default()
                .entry(store_name)
                .or_default()
                .insert(key.clone(), value_str);
        }
    });
    Ok(JsValue::from(make_request(ctx, JsValue::from(js_string!(key)))))
}

fn store_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    let Some(key_val) = args.first() else {
        return Ok(JsValue::from(make_request(ctx, JsValue::undefined())));
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let value = JS_IDB.with(|slot| {
        slot.borrow().as_ref().and_then(|state| {
            state
                .borrow()
                .get(&db_name)
                .and_then(|db| db.get(&store_name))
                .and_then(|store| store.get(&key).cloned())
        })
    });
    let result = match value {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::undefined(),
    };
    Ok(JsValue::from(make_request(ctx, result)))
}

fn store_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    if let Some(key_val) = args.first() {
        let key = key_val.to_string(ctx)?.to_std_string_escaped();
        JS_IDB.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                if let Some(db) = state.borrow_mut().get_mut(&db_name) {
                    if let Some(store) = db.get_mut(&store_name) {
                        store.remove(&key);
                    }
                }
            }
        });
    }
    Ok(JsValue::from(make_request(ctx, JsValue::undefined())))
}

fn store_clear(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    JS_IDB.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            if let Some(db) = state.borrow_mut().get_mut(&db_name) {
                if let Some(store) = db.get_mut(&store_name) {
                    store.clear();
                }
            }
        }
    });
    Ok(JsValue::from(make_request(ctx, JsValue::undefined())))
}

fn store_get_all_keys(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsArray;
    let (db_name, store_name) = store_keys(this, ctx);
    let keys: Vec<String> = JS_IDB.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| {
                state
                    .borrow()
                    .get(&db_name)
                    .and_then(|db| db.get(&store_name))
                    .map(|store| store.keys().cloned().collect())
            })
            .unwrap_or_default()
    });
    let arr = JsArray::new(ctx);
    for k in keys {
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(JsValue::from(make_request(ctx, JsValue::from(arr))))
}
