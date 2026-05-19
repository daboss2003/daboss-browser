//! IndexedDB — disk-backed.
//!
//! Each (database, store, key) maps to a real file under
//! `<data_dir>/daboss-idb/<origin>/<db>/<store>/<key-hex>`. Reads
//! and writes stream straight through the OS buffer cache; a page
//! that puts hundreds of MB of cached data into IndexedDB never
//! grows the heap, and the data survives the tab close.
//!
//! Keys are hex-encoded so arbitrary JS strings can name files
//! without colliding with the host filesystem's reserved characters.
//! Database / store names are sanitised against path traversal.
//!
//! Spec gaps we accept:
//!   * No versioning / `onupgradeneeded`. `open()` just ensures the
//!     directory exists.
//!   * No real transactions — `transaction(...).objectStore(name)`
//!     just yields a handle bound to the underlying directory.
//!   * No cursors / indexes / key paths — only string-keyed gets,
//!     puts, deletes, and `getAllKeys`.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer,
    property::Attribute, Context, JsResult, JsValue, NativeFunction,
};

thread_local! {
    /// Legacy thread-local slot retained so the engine's
    /// install/uninstall plumbing still compiles. The disk-backed
    /// IndexedDB doesn't need it; we keep the field so engine.rs
    /// doesn't have to be edited.
    pub(crate) static JS_IDB: RefCell<Option<IdbState>> = const { RefCell::new(None) };
}

pub type IdbState = std::rc::Rc<RefCell<()>>;

pub fn install(ctx: &mut Context) {
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

fn origin_root() -> PathBuf {
    let mut p = super::opfs::data_dir_path();
    p.push("daboss-idb");
    p.push(super::opfs::partitioned_origin_host());
    let _ = fs::create_dir_all(&p);
    p
}

fn db_dir(name: &str) -> PathBuf {
    let mut p = origin_root();
    p.push(super::opfs::sanitise_path_component(name));
    let _ = fs::create_dir_all(&p);
    p
}

fn store_dir(db: &str, store: &str) -> PathBuf {
    let mut p = db_dir(db);
    p.push(super::opfs::sanitise_path_component(store));
    let _ = fs::create_dir_all(&p);
    p
}

fn key_to_filename(key: &str) -> String {
    let mut out = String::with_capacity(key.len() * 2);
    for b in key.as_bytes() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn filename_to_key(name: &str) -> Option<String> {
    if name.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(name.len() / 2);
    for chunk in name.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}

fn idb_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    // Touch the directory so subsequent ops have somewhere to write.
    let _ = db_dir(&name);
    let db_obj = make_database(ctx, &name);
    let request = make_request(ctx, db_obj);
    Ok(JsValue::from(request))
}

fn idb_delete_database(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(name_val) = args.first() {
        let name = name_val.to_string(ctx)?.to_std_string_escaped();
        let dir = db_dir(&name);
        let _ = fs::remove_dir_all(&dir);
    }
    let request = make_request(ctx, JsValue::undefined());
    Ok(JsValue::from(request))
}

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
    b.accessor(
        js_string!("onsuccess"),
        None,
        Some(on_success_set),
        Attribute::ENUMERABLE,
    );
    b.property(js_string!("onerror"), JsValue::null(), Attribute::all());
    b.build()
}

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
    let target_obj = ObjectInitializer::new(ctx)
        .property(js_string!("result"), result, Attribute::READONLY)
        .build();
    let event_obj = ObjectInitializer::new(ctx)
        .property(js_string!("target"), JsValue::from(target_obj), Attribute::READONLY)
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
    b.property(
        js_string!("__db_name"),
        JsValue::from(js_string!(name.to_string())),
        Attribute::READONLY,
    );
    JsValue::from(b.build())
}

fn db_create_object_store(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let store_name = name_val.to_string(ctx)?.to_std_string_escaped();
    let db_name = read_str(this, "__db_name", ctx);
    let _ = store_dir(&db_name, &store_name);
    Ok(make_object_store_handle(ctx, &db_name, &store_name))
}

fn db_transaction(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let db_name = read_str(this, "__db_name", ctx);
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
    let db_name = read_str(this, "__db_name", ctx);
    let _ = store_dir(&db_name, &store_name);
    Ok(make_object_store_handle(ctx, &db_name, &store_name))
}

fn make_object_store_handle(ctx: &mut Context, db_name: &str, store_name: &str) -> JsValue {
    let store = ObjectInitializer::new(ctx)
        .property(
            js_string!("name"),
            JsValue::from(js_string!(store_name.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("__db_name"),
            JsValue::from(js_string!(db_name.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("__store_name"),
            JsValue::from(js_string!(store_name.to_string())),
            Attribute::READONLY,
        )
        .function(NativeFunction::from_fn_ptr(store_put), js_string!("put"), 2)
        .function(NativeFunction::from_fn_ptr(store_get), js_string!("get"), 1)
        .function(NativeFunction::from_fn_ptr(store_delete), js_string!("delete"), 1)
        .function(NativeFunction::from_fn_ptr(store_clear), js_string!("clear"), 0)
        .function(NativeFunction::from_fn_ptr(store_get_all_keys), js_string!("getAllKeys"), 0)
        .build();
    JsValue::from(store)
}

fn read_str(val: &JsValue, key: &str, ctx: &mut Context) -> String {
    val.as_object()
        .and_then(|o| o.get(js_string!(key.to_string()), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

fn store_keys(this: &JsValue, ctx: &mut Context) -> (String, String) {
    (
        read_str(this, "__db_name", ctx),
        read_str(this, "__store_name", ctx),
    )
}

fn store_put(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    let (Some(value), Some(key_val)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::from(make_request(ctx, JsValue::undefined())));
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let value_str = value.to_string(ctx)?.to_std_string_escaped();
    let mut path = store_dir(&db_name, &store_name);
    path.push(key_to_filename(&key));
    // Atomic write via sibling tempfile + rename.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, value_str.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    Ok(JsValue::from(make_request(ctx, JsValue::from(js_string!(key)))))
}

fn store_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    let Some(key_val) = args.first() else {
        return Ok(JsValue::from(make_request(ctx, JsValue::undefined())));
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let mut path = store_dir(&db_name, &store_name);
    path.push(key_to_filename(&key));
    let result = match fs::read(&path) {
        Ok(bytes) => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            JsValue::from(js_string!(s))
        }
        Err(_) => JsValue::undefined(),
    };
    Ok(JsValue::from(make_request(ctx, result)))
}

fn store_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    if let Some(key_val) = args.first() {
        let key = key_val.to_string(ctx)?.to_std_string_escaped();
        let mut path = store_dir(&db_name, &store_name);
        path.push(key_to_filename(&key));
        let _ = fs::remove_file(&path);
    }
    Ok(JsValue::from(make_request(ctx, JsValue::undefined())))
}

fn store_clear(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (db_name, store_name) = store_keys(this, ctx);
    let dir = store_dir(&db_name, &store_name);
    // remove_dir_all + recreate is the simplest atomic clear.
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    Ok(JsValue::from(make_request(ctx, JsValue::undefined())))
}

fn store_get_all_keys(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsArray;
    let (db_name, store_name) = store_keys(this, ctx);
    let dir = store_dir(&db_name, &store_name);
    let mut keys: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let file_name = entry.file_name();
            let s = match file_name.to_str() {
                Some(s) => s,
                None => continue,
            };
            // Skip in-flight tempfiles created by atomic puts.
            if s.ends_with(".tmp") {
                continue;
            }
            if let Some(key) = filename_to_key(s) {
                keys.push(key);
            }
        }
    }
    keys.sort();
    let arr = JsArray::new(ctx);
    for k in keys {
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(JsValue::from(make_request(ctx, JsValue::from(arr))))
}
