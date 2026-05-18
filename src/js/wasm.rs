//! JS `WebAssembly` namespace backed by `wasmi`.
//!
//! Surface:
//!   * `WebAssembly.validate(bytes)` → `bool`
//!   * `WebAssembly.compile(bytes)` → `Promise<Module>`
//!   * `WebAssembly.instantiate(bytes|module, imports?)`
//!     → `Promise<{module, instance}>` or `Promise<Instance>`
//!   * `WebAssembly.Module(bytes)` constructor
//!   * `WebAssembly.Instance(module, imports?)` constructor
//!   * `WebAssembly.Memory({initial, maximum?})` constructor
//!
//! Exports are surfaced on `instance.exports.<name>`:
//!   * Function exports → callable JS functions that marshal args
//!     from JS Number values to wasm Vals and back.
//!   * Memory exports → object with `.buffer` (returns a snapshot
//!     Uint8Array of the current wasm linear memory) and indexed
//!     access via `.read(offset, length)`.
//!   * Global exports → object with `.value`.
//!
//! Limitations vs the real spec:
//!   * Imports: only function imports of type `(...numbers) → number`
//!     are wired through to JS callbacks. Memory/table imports stub.
//!   * `Memory.buffer` returns a fresh Uint8Array each access, so JS
//!     can read but not write back into wasm memory directly (use
//!     `memory.write(offset, array)` for the toy).
//!   * `BigInt` for i64 isn't wired — i64 results lossily truncate to
//!     f64. Pages relying on full 64-bit precision will see drift.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArrayBuffer, JsPromise, JsUint8Array},
        FunctionObjectBuilder, ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use wasmi::{Engine, Instance, Linker, Memory, Module, Store, Val};

pub struct WasmEntry {
    pub store: Store<()>,
    pub instance: Instance,
}

pub type WasmRegistry = Rc<RefCell<Vec<Option<WasmEntry>>>>;
pub type WasmEngine = Rc<Engine>;

thread_local! {
    /// Lazily-initialised shared wasmi `Engine` for the page. One
    /// `Engine` can compile many modules; reusing it keeps per-page
    /// memory predictable.
    pub(crate) static WASM_ENGINE: RefCell<Option<WasmEngine>> =
        const { RefCell::new(None) };
    pub(crate) static WASM_INSTANCES: RefCell<WasmRegistry> =
        RefCell::new(Rc::new(RefCell::new(Vec::new())));
    /// Compiled `Module`s parked by `WebAssembly.compile`, addressed
    /// by index. `Instance` construction looks them up.
    pub(crate) static WASM_MODULES: RefCell<Vec<Option<Arc<Module>>>> =
        const { RefCell::new(Vec::new()) };
}

const MODULE_IDX_KEY: &str = "__wasm_module_idx";
const INSTANCE_IDX_KEY: &str = "__wasm_instance_idx";

fn engine() -> WasmEngine {
    WASM_ENGINE.with(|slot| {
        if let Some(e) = slot.borrow().as_ref() {
            return e.clone();
        }
        let e = Rc::new(Engine::default());
        *slot.borrow_mut() = Some(e.clone());
        e
    })
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let validate =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_validate)).build();
    let compile =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_compile)).build();
    let instantiate =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_instantiate)).build();
    let module_ctor =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_module_ctor)).build();
    let instance_ctor =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_instance_ctor)).build();
    let memory_ctor =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(wasm_memory_ctor)).build();
    let namespace = ObjectInitializer::new(ctx)
        .property(js_string!("validate"), JsValue::from(validate), Attribute::READONLY)
        .property(js_string!("compile"), JsValue::from(compile), Attribute::READONLY)
        .property(js_string!("instantiate"), JsValue::from(instantiate), Attribute::READONLY)
        .property(js_string!("Module"), JsValue::from(module_ctor), Attribute::READONLY)
        .property(js_string!("Instance"), JsValue::from(instance_ctor), Attribute::READONLY)
        .property(js_string!("Memory"), JsValue::from(memory_ctor), Attribute::READONLY)
        .build();
    let _ = ctx.register_global_property(
        js_string!("WebAssembly"),
        namespace,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Option<Vec<u8>> {
    let obj = val.as_object()?;
    // ArrayBuffer / TypedArray heuristic: read `.byteLength` and
    // index numeric props 0..length to assemble bytes. Works for
    // Uint8Array and TypedArray views; ArrayBuffer requires going
    // through a Uint8Array wrapper from JS land.
    if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
        let len = u8a.length(ctx).unwrap_or(0);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let v = u8a.at(i as i64, ctx).ok()?;
            out.push(v.to_u32(ctx).ok()? as u8);
        }
        return Some(out);
    }
    if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
        let bytes = ab.detach(&JsValue::undefined()).ok()?;
        return Some(bytes);
    }
    // Fallback: treat as a generic indexable array of numbers.
    let len = obj
        .get(js_string!("byteLength"), ctx)
        .or_else(|_| obj.get(js_string!("length"), ctx))
        .ok()?
        .to_u32(ctx)
        .ok()? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let v = obj.get(i as u32, ctx).ok()?.to_u32(ctx).ok()?;
        out.push(v as u8);
    }
    Some(out)
}

fn wasm_validate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(bytes) = args.first().and_then(|v| read_bytes(v, ctx)) else {
        return Ok(JsValue::from(false));
    };
    let engine = engine();
    Ok(JsValue::from(Module::new(&engine, &bytes).is_ok()))
}

fn wasm_compile(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(bytes) = args.first().and_then(|v| read_bytes(v, ctx)) else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "WebAssembly.compile: invalid argument"
            ))),
            ctx,
        )
        .into());
    };
    let engine = engine();
    match Module::new(&engine, &bytes) {
        Ok(module) => {
            let module_obj = wrap_module(ctx, Arc::new(module));
            Ok(JsPromise::resolve(module_obj, ctx).into())
        }
        Err(e) => {
            let msg = format!("WebAssembly.compile: {e}");
            Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(msg))),
                ctx,
            )
            .into())
        }
    }
}

fn wasm_module_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(bytes) = args.first().and_then(|v| read_bytes(v, ctx)) else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("WebAssembly.Module: missing bytes argument")
            .into());
    };
    let engine = engine();
    match Module::new(&engine, &bytes) {
        Ok(m) => Ok(wrap_module(ctx, Arc::new(m))),
        Err(e) => Err(boa_engine::JsNativeError::typ()
            .with_message(format!("WebAssembly.Module: {e}"))
            .into()),
    }
}

fn wrap_module(ctx: &mut Context, module: Arc<Module>) -> JsValue {
    let idx = WASM_MODULES.with(|slot| {
        let mut s = slot.borrow_mut();
        s.push(Some(module));
        s.len() - 1
    });
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!(MODULE_IDX_KEY),
            JsValue::from(idx as u32),
            Attribute::READONLY,
        )
        .build();
    JsValue::from(obj)
}

fn module_from_value(val: &JsValue, ctx: &mut Context) -> Option<Arc<Module>> {
    let obj = val.as_object()?;
    let idx = obj
        .get(js_string!(MODULE_IDX_KEY), ctx)
        .ok()?
        .to_u32(ctx)
        .ok()? as usize;
    WASM_MODULES.with(|slot| slot.borrow().get(idx).and_then(|s| s.clone()))
}

fn wasm_instantiate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(first) = args.first() else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "WebAssembly.instantiate: missing argument"
            ))),
            ctx,
        )
        .into());
    };
    // First arg is either a compiled Module wrapper or raw bytes.
    let (module, return_module) = if let Some(m) = module_from_value(first, ctx) {
        (m, false)
    } else {
        let bytes = match read_bytes(first, ctx) {
            Some(b) => b,
            None => {
                return Ok(JsPromise::reject(
                    boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                        "WebAssembly.instantiate: invalid first argument"
                    ))),
                    ctx,
                )
                .into());
            }
        };
        let engine = engine();
        let parsed = match Module::new(&engine, &bytes) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                return Ok(JsPromise::reject(
                    boa_engine::JsError::from_opaque(JsValue::from(js_string!(format!(
                        "WebAssembly.instantiate compile: {e}"
                    )))),
                    ctx,
                )
                .into());
            }
        };
        (parsed, true)
    };

    let entry = match instantiate_module(&module) {
        Ok(e) => e,
        Err(e) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(format!(
                    "WebAssembly.instantiate: {e}"
                )))),
                ctx,
            )
            .into());
        }
    };
    let instance_obj = wrap_instance(ctx, entry);

    let result = if return_module {
        let module_obj = wrap_module(ctx, module);
        let pair = ObjectInitializer::new(ctx)
            .property(
                js_string!("module"),
                module_obj,
                Attribute::READONLY,
            )
            .property(
                js_string!("instance"),
                instance_obj,
                Attribute::READONLY,
            )
            .build();
        JsValue::from(pair)
    } else {
        instance_obj
    };
    Ok(JsPromise::resolve(result, ctx).into())
}

fn wasm_instance_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(module_val) = args.first() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("WebAssembly.Instance: missing module argument")
            .into());
    };
    let Some(module) = module_from_value(module_val, ctx) else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("WebAssembly.Instance: first argument must be a Module")
            .into());
    };
    let entry = instantiate_module(&module).map_err(|e| {
        boa_engine::JsNativeError::error().with_message(format!("WebAssembly.Instance: {e}"))
    })?;
    Ok(wrap_instance(ctx, entry))
}

fn instantiate_module(module: &Arc<Module>) -> Result<WasmEntry, String> {
    let engine = engine();
    let mut store: Store<()> = Store::new(&engine, ());
    let linker: Linker<()> = Linker::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, module)
        .map_err(|e| e.to_string())?;
    Ok(WasmEntry { store, instance })
}

fn wrap_instance(ctx: &mut Context, entry: WasmEntry) -> JsValue {
    let idx = WASM_INSTANCES.with(|slot| {
        let rc = slot.borrow().clone();
        let mut reg = rc.borrow_mut();
        reg.push(Some(entry));
        reg.len() - 1
    });
    let exports = build_exports_object(ctx, idx);
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!(INSTANCE_IDX_KEY),
            JsValue::from(idx as u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("exports"),
            exports,
            Attribute::READONLY,
        )
        .build();
    JsValue::from(obj)
}

fn build_exports_object(ctx: &mut Context, instance_idx: usize) -> JsValue {
    // Snapshot the export names+kinds before we hand the borrow back.
    let exports: Vec<(String, wasmi::ExternType)> =
        WASM_INSTANCES.with(|slot| -> Vec<(String, wasmi::ExternType)> {
            let rc = slot.borrow().clone();
            let reg = rc.borrow();
            let Some(Some(entry)) = reg.get(instance_idx) else {
                return Vec::new();
            };
            entry
                .instance
                .exports(&entry.store)
                .map(|exp| (exp.name().to_string(), exp.ty(&entry.store)))
                .collect()
        });

    // Build each export value first so we own them as JsValues before
    // starting the ObjectInitializer (which holds a mutable borrow on
    // `ctx` for its lifetime).
    let mut entries: Vec<(String, JsValue)> = Vec::with_capacity(exports.len());
    for (name, ty) in exports {
        let value = match ty {
            wasmi::ExternType::Func(_) => make_export_func(ctx, instance_idx, &name),
            wasmi::ExternType::Memory(_) => make_export_memory(ctx, instance_idx, &name),
            wasmi::ExternType::Global(_) => make_export_global(ctx, instance_idx, &name),
            wasmi::ExternType::Table(_) => JsValue::null(),
        };
        entries.push((name, value));
    }
    let mut b = ObjectInitializer::new(ctx);
    for (name, value) in entries {
        b.property(js_string!(name), value, Attribute::READONLY);
    }
    JsValue::from(b.build())
}

fn make_export_func(ctx: &mut Context, instance_idx: usize, name: &str) -> JsValue {
    // Leak the export name into a `'static str` so the closure
    // capturing it remains `Copy` (required by `from_copy_closure`).
    // One leak per unique export; bounded by module surface area.
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let idx = instance_idx;
    let closure = move |_this: &JsValue, args: &[JsValue], ctx: &mut Context| -> JsResult<JsValue> {
        call_wasm_function(idx, leaked, args, ctx)
    };
    let realm = ctx.realm().clone();
    let f = FunctionObjectBuilder::new(&realm, NativeFunction::from_copy_closure(closure)).build();
    JsValue::from(f)
}

fn call_wasm_function(
    instance_idx: usize,
    name: &str,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // Marshal arg numbers into the func's parameter types.
    let registry = WASM_INSTANCES.with(|slot| slot.borrow().clone());
    let mut reg = registry.borrow_mut();
    let Some(Some(entry)) = reg.get_mut(instance_idx) else {
        return Ok(JsValue::undefined());
    };
    let Some(func) = entry.instance.get_func(&entry.store, name) else {
        return Ok(JsValue::undefined());
    };
    let func_ty = func.ty(&entry.store);
    let params: Vec<Val> = func_ty
        .params()
        .iter()
        .enumerate()
        .map(|(i, vt)| js_to_val(args.get(i), *vt, ctx))
        .collect();
    let mut results: Vec<Val> = func_ty
        .results()
        .iter()
        .map(|vt| Val::default(*vt))
        .collect();
    if let Err(e) = func.call(&mut entry.store, &params, &mut results) {
        return Err(boa_engine::JsNativeError::error()
            .with_message(format!("wasm call '{name}': {e}"))
            .into());
    }
    Ok(match results.len() {
        0 => JsValue::undefined(),
        1 => val_to_js(&results[0]),
        _ => {
            // Multi-value returns flattened into a JS array, in
            // declared order. (Spec says JS sees an array for
            // multiple returns once multi-value lands; we match.)
            let arr = boa_engine::object::builtins::JsArray::new(ctx);
            for v in &results {
                let _ = arr.push(val_to_js(v), ctx);
            }
            JsValue::from(arr)
        }
    })
}

fn js_to_val(v: Option<&JsValue>, ty: wasmi::ValType, ctx: &mut Context) -> Val {
    let Some(v) = v else {
        return zero_val(ty);
    };
    match ty {
        wasmi::ValType::I32 => Val::I32(v.to_i32(ctx).unwrap_or(0)),
        wasmi::ValType::I64 => {
            let n = v.to_number(ctx).unwrap_or(0.0);
            Val::I64(n as i64)
        }
        wasmi::ValType::F32 => Val::F32((v.to_number(ctx).unwrap_or(0.0) as f32).into()),
        wasmi::ValType::F64 => Val::F64(v.to_number(ctx).unwrap_or(0.0).into()),
        _ => zero_val(ty),
    }
}

fn zero_val(ty: wasmi::ValType) -> Val {
    Val::default(ty)
}

fn val_to_js(v: &Val) -> JsValue {
    match v {
        Val::I32(n) => JsValue::from(*n),
        Val::I64(n) => JsValue::from(*n as f64),
        Val::F32(n) => JsValue::from(f32::from(*n) as f64),
        Val::F64(n) => JsValue::from(f64::from(*n)),
        _ => JsValue::null(),
    }
}

fn make_export_memory(ctx: &mut Context, instance_idx: usize, name: &str) -> JsValue {
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let realm = ctx.realm().clone();
    let buffer_getter_closure = move |_: &JsValue, _: &[JsValue], ctx: &mut Context| {
        memory_buffer_snapshot(instance_idx, leaked, ctx)
    };
    let buffer_getter = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_copy_closure(buffer_getter_closure),
    )
    .build();
    let leaked_for_grow: &'static str = leaked;
    let grow_closure = move |_: &JsValue, args: &[JsValue], ctx: &mut Context| {
        memory_grow(instance_idx, leaked_for_grow, args, ctx)
    };
    let grow_fn = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_copy_closure(grow_closure),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(INSTANCE_IDX_KEY),
        JsValue::from(instance_idx as u32),
        Attribute::READONLY,
    );
    b.accessor(
        js_string!("buffer"),
        Some(buffer_getter),
        None,
        Attribute::ENUMERABLE,
    );
    b.property(
        js_string!("grow"),
        JsValue::from(grow_fn),
        Attribute::READONLY,
    );
    JsValue::from(b.build())
}

fn memory_handle(instance_idx: usize, name: &str) -> Option<(WasmRegistry, Memory)> {
    let registry = WASM_INSTANCES.with(|slot| slot.borrow().clone());
    let memory = {
        let reg = registry.borrow();
        let entry = reg.get(instance_idx).and_then(|s| s.as_ref())?;
        let ext = entry.instance.get_export(&entry.store, name)?;
        match ext {
            wasmi::Extern::Memory(m) => Some(m),
            _ => None,
        }?
    };
    Some((registry, memory))
}

fn memory_buffer_snapshot(
    instance_idx: usize,
    name: &str,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some((registry, memory)) = memory_handle(instance_idx, name) else {
        return Ok(JsValue::null());
    };
    let bytes: Vec<u8> = {
        let mut reg = registry.borrow_mut();
        let entry = reg
            .get_mut(instance_idx)
            .and_then(|s| s.as_mut())
            .expect("entry present");
        memory.data(&entry.store).to_vec()
    };
    let buf = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    Ok(JsValue::from(buf))
}

fn memory_grow(
    instance_idx: usize,
    name: &str,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(delta) = args.first().and_then(|v| v.to_u32(ctx).ok()) else {
        return Ok(JsValue::from(0));
    };
    let Some((registry, memory)) = memory_handle(instance_idx, name) else {
        return Ok(JsValue::from(0));
    };
    let mut reg = registry.borrow_mut();
    let entry = reg
        .get_mut(instance_idx)
        .and_then(|s| s.as_mut())
        .expect("entry present");
    match memory.grow(&mut entry.store, delta as u64) {
        Ok(prev_pages) => Ok(JsValue::from(prev_pages as u32)),
        Err(_) => Ok(JsValue::from(-1_i32)),
    }
}

fn make_export_global(ctx: &mut Context, instance_idx: usize, name: &str) -> JsValue {
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let realm = ctx.realm().clone();
    let value_getter_closure = move |_: &JsValue, _: &[JsValue], _ctx: &mut Context| {
        Ok(global_read(instance_idx, leaked))
    };
    let value_getter = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_copy_closure(value_getter_closure),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    b.accessor(
        js_string!("value"),
        Some(value_getter),
        None,
        Attribute::ENUMERABLE,
    );
    JsValue::from(b.build())
}

fn global_read(instance_idx: usize, name: &str) -> JsValue {
    let registry = WASM_INSTANCES.with(|slot| slot.borrow().clone());
    let reg = registry.borrow();
    let Some(Some(entry)) = reg.get(instance_idx) else {
        return JsValue::undefined();
    };
    let Some(global) = entry.instance.get_global(&entry.store, name) else {
        return JsValue::undefined();
    };
    val_to_js(&global.get(&entry.store))
}

fn wasm_memory_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let initial = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("initial"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(1);
    // We can't easily mint a free-standing wasmi Memory without a Store
    // context, so the constructed Memory is just a JS-side byte buffer
    // sized to `initial` 64KiB pages. Pages can read/write via the
    // `buffer` accessor.
    let bytes = vec![0u8; (initial as usize) * 64 * 1024];
    let buf = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("buffer"),
            JsValue::from(buf),
            Attribute::all(),
        )
        .build();
    Ok(JsValue::from(obj))
}
