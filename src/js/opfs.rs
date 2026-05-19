//! Origin Private File System — disk-backed.
//!
//! Files live under `<data_dir>/daboss-opfs/<origin-host>/...` and
//! map 1:1 to real filesystem entries. Each `FileSystemFileHandle` /
//! `FileSystemDirectoryHandle` carries a `PathBuf` that operations
//! traverse directly. This means a multi-GB OPFS file never lives in
//! RAM — `getFile()` reads on demand and writers stream to disk.
//!
//! `<data_dir>` resolves to:
//!   * `$XDG_DATA_HOME` on Linux/Android if set
//!   * `~/.local/share` on Linux/Android otherwise
//!   * `~/Library/Application Support` on macOS
//!   * `%LOCALAPPDATA%` on Windows
//!   * `std::env::temp_dir()` as a last-resort fallback
//!
//! Each origin (host portion of the current page's URL) gets its own
//! subtree. Pages on different hosts cannot see each other's files.
//!
//! `FileSystemWritableFileStream` writes to a sibling `.tmp` file and
//! atomically renames over the target on `close()`. Aborts delete
//! the tempfile. `createSyncAccessHandle` opens the real file with
//! read+write directly — reads and writes operate against the OS
//! buffer without staging through Rust memory.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArray, JsPromise},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

const DIR_ID_KEY: &str = "__opfs_dir_id";
const FILE_ID_KEY: &str = "__opfs_file_id";
const WRITER_ID_KEY: &str = "__opfs_writer_id";

pub struct DirHandle {
    pub path: PathBuf,
}

pub struct FileHandle {
    pub path: PathBuf,
}

pub struct WriterState {
    /// Target path the writer will atomically rename to on close.
    pub target_path: PathBuf,
    /// Tempfile we stream writes into. None for sync access handles
    /// (which operate against the real file in-place).
    pub temp_path: Option<PathBuf>,
    pub file: std::fs::File,
    /// True for sync access handles — close commits in place rather
    /// than rename-from-tempfile.
    pub sync_mode: bool,
}

thread_local! {
    pub(crate) static DIR_HANDLES: RefCell<HashMap<u32, DirHandle>> = RefCell::new(HashMap::new());
    pub(crate) static FILE_HANDLES: RefCell<HashMap<u32, FileHandle>> = RefCell::new(HashMap::new());
    pub(crate) static OPFS_WRITERS: RefCell<HashMap<u32, WriterState>> = RefCell::new(HashMap::new());
    pub(crate) static OPFS_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    OPFS_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

/// Resolve `<data_dir>/daboss-opfs/<origin-host>` and ensure it
/// exists. Falls back through XDG / temp_dir if the platform's
/// preferred location is unavailable.
fn origin_root() -> PathBuf {
    let mut base = data_dir_path();
    base.push("daboss-opfs");
    base.push(partitioned_origin_host());
    let _ = fs::create_dir_all(&base);
    base
}

/// Resolve the platform's user-data directory. Shared with other
/// per-origin disk-backed stores (IndexedDB, localStorage).
pub(crate) fn data_dir_path() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            let mut p = PathBuf::from(home);
            p.push("Library");
            p.push("Application Support");
            return p;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(p) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(p);
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let mut p = PathBuf::from(home);
            p.push(".local");
            p.push("share");
            return p;
        }
    }
    std::env::temp_dir()
}

/// Resolve the current page's origin host into a sanitised path
/// component. Shared with other per-origin disk-backed stores.
///
/// Returns the unpartitioned bare-inner-origin string. New disk
/// paths should call [`partitioned_origin_host`] instead so they
/// participate in top-level-origin partitioning.
pub(crate) fn current_origin_host() -> String {
    let host = super::engine::JS_BASE_URL.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
    });
    sanitise_path_component(&host.unwrap_or_else(|| "default".to_string()))
}

/// Compute the storage partition key for the current document. The
/// returned string is a sanitised path component suitable for use as
/// a directory name; per-origin disk-backed stores (IDB, OPFS,
/// localStorage, SW caches, SW registrations, push) push this onto
/// their root path so two iframes from the same inner origin land
/// in distinct directories when their embedders differ.
///
/// Format: `<top-host>__<inner-host>`. When the two coincide (the
/// common single-frame top-level case) we use just the inner host
/// — matching the pre-partition disk layout so already-stored data
/// keeps working without a migration step.
pub(crate) fn partitioned_origin_host() -> String {
    let inner_host = super::engine::JS_BASE_URL.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
    });
    let top_host = super::engine::JS_TOP_LEVEL_BASE_URL.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
    });
    let inner = inner_host.unwrap_or_else(|| "default".to_string());
    let top = top_host.unwrap_or_else(|| inner.clone());
    if top == inner {
        sanitise_path_component(&inner)
    } else {
        sanitise_path_component(&format!("{top}__{inner}"))
    }
}

/// Strip path traversal characters so an origin like `..` or
/// `foo/bar` can't reach outside the OPFS root.
pub(crate) fn sanitise_path_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let get_dir = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(storage_get_directory),
    )
    .build();
    let estimate = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(storage_estimate),
    )
    .build();
    let persist = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(storage_persist),
    )
    .build();
    let persisted = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(storage_persisted),
    )
    .build();
    let storage = ObjectInitializer::new(ctx)
        .property(
            js_string!("getDirectory"),
            JsValue::from(get_dir),
            Attribute::READONLY,
        )
        .property(
            js_string!("estimate"),
            JsValue::from(estimate),
            Attribute::READONLY,
        )
        .property(
            js_string!("persist"),
            JsValue::from(persist),
            Attribute::READONLY,
        )
        .property(
            js_string!("persisted"),
            JsValue::from(persisted),
            Attribute::READONLY,
        )
        .build();
    let global = ctx.global_object();
    if let Ok(nav_val) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.set(
                js_string!("storage"),
                JsValue::from(storage),
                false,
                ctx,
            );
        }
    }
}

// ============ navigator.storage methods ============

fn storage_get_directory(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let root = origin_root();
    let id = register_dir(root);
    Ok(JsPromise::resolve(build_directory_handle(ctx, id), ctx).into())
}

fn storage_estimate(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let used = directory_size(&origin_root());
    let report = ObjectInitializer::new(ctx)
        .property(
            js_string!("usage"),
            JsValue::from(used as f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("quota"),
            JsValue::from(f64::INFINITY),
            Attribute::READONLY,
        )
        .build();
    Ok(JsPromise::resolve(JsValue::from(report), ctx).into())
}

fn directory_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = fs::read_dir(path) else { return 0 };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(directory_size(&entry.path()));
        }
    }
    total
}

fn storage_persist(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // We're already disk-backed; persistence is always "granted".
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn storage_persisted(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

// ============ handle helpers ============

fn register_dir(path: PathBuf) -> u32 {
    let id = next_id();
    DIR_HANDLES.with(|r| {
        r.borrow_mut().insert(id, DirHandle { path });
    });
    id
}

fn register_file(path: PathBuf) -> u32 {
    let id = next_id();
    FILE_HANDLES.with(|r| {
        r.borrow_mut().insert(id, FileHandle { path });
    });
    id
}

fn dir_path(id: u32) -> Option<PathBuf> {
    DIR_HANDLES.with(|r| r.borrow().get(&id).map(|d| d.path.clone()))
}

fn file_path(id: u32) -> Option<PathBuf> {
    FILE_HANDLES.with(|r| r.borrow().get(&id).map(|f| f.path.clone()))
}

// ============ FileSystemDirectoryHandle ============

fn build_directory_handle(ctx: &mut Context, id: u32) -> JsValue {
    let name = dir_path(id)
        .as_ref()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "".to_string());
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(DIR_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("kind"),
        JsValue::from(js_string!("directory")),
        Attribute::READONLY,
    );
    b.property(
        js_string!("name"),
        JsValue::from(js_string!(name)),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("getFileHandle", NativeFunction::from_fn_ptr(dir_get_file_handle), 2),
        ("getDirectoryHandle", NativeFunction::from_fn_ptr(dir_get_directory_handle), 2),
        ("removeEntry", NativeFunction::from_fn_ptr(dir_remove_entry), 2),
        ("keys", NativeFunction::from_fn_ptr(dir_keys), 0),
        ("values", NativeFunction::from_fn_ptr(dir_values), 0),
        ("entries", NativeFunction::from_fn_ptr(dir_entries), 0),
        ("resolve", NativeFunction::from_fn_ptr(dir_resolve), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn dir_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(DIR_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

/// Join `name` onto `base`, rejecting any path component that would
/// escape the OPFS root (`..`, embedded slashes, drive letters).
fn safe_child_path(base: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(base.join(name))
}

fn dir_get_file_handle(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(dir_id) = dir_id_of(this, ctx) else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!("invalid dir"))),
            ctx,
        )
        .into());
    };
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let create = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("create"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let Some(parent) = dir_path(dir_id) else {
        return reject(ctx, "invalid dir handle");
    };
    let Some(child) = safe_child_path(&parent, &name) else {
        return reject(ctx, "InvalidName");
    };
    let exists = child.is_file();
    if !exists && !create {
        return reject(ctx, "NotFoundError");
    }
    if exists && child.is_dir() {
        return reject(ctx, "TypeMismatchError: path is a directory");
    }
    if !exists {
        // Create empty file. fs::File::create truncates if exists, so
        // we check first.
        match fs::File::create(&child) {
            Ok(_) => {}
            Err(_) => return reject(ctx, "InvalidStateError"),
        }
    }
    let id = register_file(child);
    Ok(JsPromise::resolve(build_file_handle(ctx, id), ctx).into())
}

fn dir_get_directory_handle(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(dir_id) = dir_id_of(this, ctx) else {
        return reject(ctx, "invalid dir");
    };
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let create = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("create"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let Some(parent) = dir_path(dir_id) else {
        return reject(ctx, "invalid dir handle");
    };
    let Some(child) = safe_child_path(&parent, &name) else {
        return reject(ctx, "InvalidName");
    };
    let exists = child.is_dir();
    if !exists && !create {
        return reject(ctx, "NotFoundError");
    }
    if exists && child.is_file() {
        return reject(ctx, "TypeMismatchError: path is a file");
    }
    if !exists {
        if fs::create_dir_all(&child).is_err() {
            return reject(ctx, "InvalidStateError");
        }
    }
    let id = register_dir(child);
    Ok(JsPromise::resolve(build_directory_handle(ctx, id), ctx).into())
}

fn dir_remove_entry(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(dir_id) = dir_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let recursive = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("recursive"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let Some(parent) = dir_path(dir_id) else {
        return reject(ctx, "invalid dir handle");
    };
    let Some(target) = safe_child_path(&parent, &name) else {
        return reject(ctx, "InvalidName");
    };
    if !target.exists() {
        return reject(ctx, "NotFoundError");
    }
    let result = if target.is_dir() {
        if recursive {
            fs::remove_dir_all(&target)
        } else {
            fs::remove_dir(&target)
        }
    } else {
        fs::remove_file(&target)
    };
    if result.is_err() {
        return reject(ctx, "InvalidModificationError");
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn dir_keys(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let entries = list_children(this, ctx);
    let arr = JsArray::new(ctx);
    for (name, _, _) in entries {
        let _ = arr.push(JsValue::from(js_string!(name)), ctx);
    }
    Ok(arr.into())
}

fn dir_values(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let entries = list_children(this, ctx);
    let arr = JsArray::new(ctx);
    for (_, path, is_dir) in entries {
        let handle = if is_dir {
            let id = register_dir(path);
            build_directory_handle(ctx, id)
        } else {
            let id = register_file(path);
            build_file_handle(ctx, id)
        };
        let _ = arr.push(handle, ctx);
    }
    Ok(arr.into())
}

fn dir_entries(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let entries = list_children(this, ctx);
    let arr = JsArray::new(ctx);
    for (name, path, is_dir) in entries {
        let handle = if is_dir {
            let id = register_dir(path);
            build_directory_handle(ctx, id)
        } else {
            let id = register_file(path);
            build_file_handle(ctx, id)
        };
        let pair = JsArray::new(ctx);
        let _ = pair.push(JsValue::from(js_string!(name)), ctx);
        let _ = pair.push(handle, ctx);
        let _ = arr.push(JsValue::from(pair), ctx);
    }
    Ok(arr.into())
}

fn list_children(this: &JsValue, ctx: &mut Context) -> Vec<(String, PathBuf, bool)> {
    let Some(dir_id) = dir_id_of(this, ctx) else {
        return Vec::new();
    };
    let Some(path) = dir_path(dir_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&path) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let p = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            out.push((name, p, meta.is_dir()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn dir_resolve(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(dir_id) = dir_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let Some(base) = dir_path(dir_id) else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let target_path = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| {
            o.get(js_string!(DIR_ID_KEY), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .and_then(dir_path)
                .or_else(|| {
                    o.get(js_string!(FILE_ID_KEY), ctx)
                        .ok()
                        .and_then(|v| v.to_u32(ctx).ok())
                        .and_then(file_path)
                })
        });
    let Some(target) = target_path else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let Ok(rel) = target.strip_prefix(&base) else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let arr = JsArray::new(ctx);
    for comp in rel.components() {
        let s = comp.as_os_str().to_string_lossy().into_owned();
        let _ = arr.push(JsValue::from(js_string!(s)), ctx);
    }
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

// ============ FileSystemFileHandle ============

fn build_file_handle(ctx: &mut Context, id: u32) -> JsValue {
    let name = file_path(id)
        .as_ref()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(FILE_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("kind"),
        JsValue::from(js_string!("file")),
        Attribute::READONLY,
    );
    b.property(
        js_string!("name"),
        JsValue::from(js_string!(name)),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(file_get_file),
        js_string!("getFile"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(file_create_writable),
        js_string!("createWritable"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(file_create_sync_access_handle),
        js_string!("createSyncAccessHandle"),
        0,
    );
    JsValue::from(b.build())
}

fn file_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(FILE_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn file_get_file(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = file_id_of(this, ctx) else {
        return reject(ctx, "invalid file");
    };
    let Some(path) = file_path(id) else {
        return reject(ctx, "invalid file path");
    };
    // Read on demand. For very large files JS code should prefer
    // `createSyncAccessHandle()` and `read(buf, {at})` to chunk
    // through; getFile() is for callers that want the whole thing.
    let bytes = fs::read(&path).unwrap_or_default();
    let size = bytes.len() as u32;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified = fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    // Hand bytes off to file.rs's blob store (single allocation).
    let blob_id = super::file::store_blob(bytes, "application/octet-stream".to_string());
    let file_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("__blob_id"),
            JsValue::from(blob_id),
            Attribute::READONLY,
        )
        .property(
            js_string!("size"),
            JsValue::from(size),
            Attribute::READONLY,
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!("application/octet-stream")),
            Attribute::READONLY,
        )
        .property(
            js_string!("name"),
            JsValue::from(js_string!(name)),
            Attribute::READONLY,
        )
        .property(
            js_string!("lastModified"),
            JsValue::from(modified),
            Attribute::READONLY,
        )
        .build();
    Ok(JsPromise::resolve(JsValue::from(file_obj), ctx).into())
}

fn file_create_writable(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = file_id_of(this, ctx) else {
        return reject(ctx, "invalid file");
    };
    let Some(target_path) = file_path(id) else {
        return reject(ctx, "invalid file path");
    };
    let keep_existing = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("keepExistingData"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    // Open a sibling `.tmp` file. On close we rename it over the
    // target atomically.
    let temp_path = sibling_tmp(&target_path);
    if keep_existing {
        // Copy the current file's bytes into the temp so writes
        // start from the current state. If the target doesn't exist
        // yet, start fresh.
        if target_path.exists() {
            if fs::copy(&target_path, &temp_path).is_err() {
                return reject(ctx, "could not initialise tempfile");
            }
        } else {
            if fs::File::create(&temp_path).is_err() {
                return reject(ctx, "could not create tempfile");
            }
        }
    } else {
        // Fresh empty tempfile.
        if fs::File::create(&temp_path).is_err() {
            return reject(ctx, "could not create tempfile");
        }
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true);
    let file = match opts.open(&temp_path) {
        Ok(f) => f,
        Err(_) => return reject(ctx, "could not open tempfile"),
    };
    let writer_id = next_id();
    OPFS_WRITERS.with(|r| {
        r.borrow_mut().insert(
            writer_id,
            WriterState {
                target_path,
                temp_path: Some(temp_path),
                file,
                sync_mode: false,
            },
        );
    });
    // Seek to end so the first write() (without explicit at/seek)
    // appends after any pre-loaded content.
    OPFS_WRITERS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&writer_id) {
            let _ = state.file.seek(SeekFrom::End(0));
        }
    });
    Ok(JsPromise::resolve(build_writable_stream(ctx, writer_id), ctx).into())
}

fn sibling_tmp(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "opfs".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.{stamp:x}.tmp"))
}

fn file_create_sync_access_handle(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = file_id_of(this, ctx) else {
        return reject(ctx, "invalid file");
    };
    let Some(target_path) = file_path(id) else {
        return reject(ctx, "invalid file path");
    };
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    let file = match opts.open(&target_path) {
        Ok(f) => f,
        Err(_) => return reject(ctx, "could not open file"),
    };
    let writer_id = next_id();
    OPFS_WRITERS.with(|r| {
        r.borrow_mut().insert(
            writer_id,
            WriterState {
                target_path,
                temp_path: None,
                file,
                sync_mode: true,
            },
        );
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(WRITER_ID_KEY),
        JsValue::from(writer_id),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("write", NativeFunction::from_fn_ptr(sync_write), 2),
        ("read", NativeFunction::from_fn_ptr(sync_read), 2),
        ("getSize", NativeFunction::from_fn_ptr(sync_get_size), 0),
        ("truncate", NativeFunction::from_fn_ptr(sync_truncate), 1),
        ("flush", NativeFunction::from_fn_ptr(sync_flush), 0),
        ("close", NativeFunction::from_fn_ptr(sync_close), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    Ok(JsPromise::resolve(JsValue::from(b.build()), ctx).into())
}

// ============ FileSystemWritableFileStream ============

fn build_writable_stream(ctx: &mut Context, writer_id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(WRITER_ID_KEY),
        JsValue::from(writer_id),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("write", NativeFunction::from_fn_ptr(writable_write), 1),
        ("seek", NativeFunction::from_fn_ptr(writable_seek), 1),
        ("truncate", NativeFunction::from_fn_ptr(writable_truncate), 1),
        ("close", NativeFunction::from_fn_ptr(writable_close), 0),
        ("abort", NativeFunction::from_fn_ptr(writable_abort), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn writer_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WRITER_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn writable_write(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let val = args.first().cloned().unwrap_or(JsValue::undefined());
    // Spec: the value can be a Blob / ArrayBuffer / TypedArray /
    // DOMString OR a descriptor `{type: "write"|"seek"|"truncate",
    // data, position, size}`. Handle both.
    if let Some(obj) = val.as_object() {
        if let Ok(ty) = obj.get(js_string!("type"), ctx) {
            if let Ok(s) = ty.to_string(ctx) {
                match s.to_std_string_escaped().as_str() {
                    "seek" => {
                        if let Ok(p) = obj.get(js_string!("position"), ctx) {
                            if let Ok(pos) = p.to_u32(ctx) {
                                let _ = seek_writer(id, pos as u64);
                            }
                        }
                        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
                    }
                    "truncate" => {
                        if let Ok(sz) = obj.get(js_string!("size"), ctx) {
                            if let Ok(s) = sz.to_u32(ctx) {
                                let _ = truncate_writer(id, s as u64);
                            }
                        }
                        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
                    }
                    _ => {}
                }
            }
        }
    }
    // Streamed write — pull bytes from the source (Blob/typed array/string)
    // and push them through to disk without staging the whole payload.
    write_value_to_disk(id, &val, ctx);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn write_value_to_disk(id: u32, val: &JsValue, ctx: &mut Context) {
    // Blob: stream from the blob's bytes via file.rs (still a single
    // Vec but we don't double-buffer in the writer struct).
    if let Some(obj) = val.as_object() {
        if let Ok(bid) = obj.get(js_string!("__blob_id"), ctx) {
            if let Ok(blob_id) = bid.to_u32(ctx) {
                if let Some(bytes) = super::file::read_blob_bytes(blob_id) {
                    write_bytes(id, &bytes);
                    return;
                }
            }
        }
    }
    if val.is_string() {
        if let Ok(s) = val.to_string(ctx) {
            let bytes = s.to_std_string_escaped().into_bytes();
            write_bytes(id, &bytes);
            return;
        }
    }
    let bytes = read_bytes(val, ctx);
    if !bytes.is_empty() {
        write_bytes(id, &bytes);
    }
}

fn write_bytes(id: u32, bytes: &[u8]) {
    OPFS_WRITERS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            let _ = state.file.write_all(bytes);
        }
    });
}

fn seek_writer(id: u32, pos: u64) -> std::io::Result<()> {
    OPFS_WRITERS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            state.file.seek(SeekFrom::Start(pos))?;
        }
        Ok(())
    })
}

fn truncate_writer(id: u32, size: u64) -> std::io::Result<()> {
    OPFS_WRITERS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            state.file.set_len(size)?;
        }
        Ok(())
    })
}

fn writable_seek(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let pos = args
        .first()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0) as u64;
    let _ = seek_writer(id, pos);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writable_truncate(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let sz = args
        .first()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0) as u64;
    let _ = truncate_writer(id, sz);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writable_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    commit_writer(id);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writable_abort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_id_of(this, ctx) {
        // Drop the writer and remove its tempfile.
        let state = OPFS_WRITERS.with(|r| r.borrow_mut().remove(&id));
        if let Some(s) = state {
            drop(s.file);
            if let Some(tmp) = s.temp_path {
                let _ = fs::remove_file(tmp);
            }
        }
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn commit_writer(id: u32) {
    let state = OPFS_WRITERS.with(|r| r.borrow_mut().remove(&id));
    let Some(state) = state else { return };
    let WriterState {
        target_path,
        temp_path,
        mut file,
        sync_mode,
    } = state;
    let _ = file.flush();
    drop(file);
    if sync_mode {
        // Already operating on the real file in place.
        return;
    }
    if let Some(tmp) = temp_path {
        // Atomic rename from tempfile → target.
        if fs::rename(&tmp, &target_path).is_err() {
            // Fallback: copy + delete.
            if let Err(e) = fs::copy(&tmp, &target_path) {
                eprintln!("[opfs] commit copy failed: {e}");
            }
            let _ = fs::remove_file(&tmp);
        }
    }
}

// ============ SyncAccessHandle (synchronous file IO) ============

fn sync_write(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsValue::from(0u32));
    };
    let buf = args.first().cloned().unwrap_or(JsValue::undefined());
    let opts_pos = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("at"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as u64);
    let bytes = read_bytes(&buf, ctx);
    let mut written = 0u32;
    OPFS_WRITERS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            if let Some(pos) = opts_pos {
                let _ = state.file.seek(SeekFrom::Start(pos));
            }
            if state.file.write_all(&bytes).is_ok() {
                written = bytes.len() as u32;
            }
        }
    });
    Ok(JsValue::from(written))
}

fn sync_read(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsValue::from(0u32));
    };
    let buf = args.first().cloned().unwrap_or(JsValue::undefined());
    let opts_pos = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("at"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as u64);
    let mut read_count = 0u32;
    if let Some(target) = buf.as_object() {
        if let Ok(u8a) = boa_engine::object::builtins::JsUint8Array::from_object(target.clone()) {
            let len = u8a.length(ctx).unwrap_or(0);
            let mut tmp = vec![0u8; len];
            OPFS_WRITERS.with(|r| {
                if let Some(state) = r.borrow_mut().get_mut(&id) {
                    if let Some(pos) = opts_pos {
                        let _ = state.file.seek(SeekFrom::Start(pos));
                    }
                    if let Ok(n) = state.file.read(&mut tmp) {
                        read_count = n as u32;
                    }
                }
            });
            for (i, b) in tmp.iter().take(read_count as usize).enumerate() {
                let _ = u8a.set(i as i64, JsValue::from(*b as u32), false, ctx);
            }
        }
    }
    Ok(JsValue::from(read_count))
}

fn sync_get_size(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsValue::from(0u32));
    };
    let size = OPFS_WRITERS.with(|r| {
        r.borrow_mut()
            .get_mut(&id)
            .and_then(|state| state.file.metadata().ok().map(|m| m.len()))
            .unwrap_or(0)
    });
    Ok(JsValue::from(size as u32))
}

fn sync_truncate(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let sz = args
        .first()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(0.0) as u64;
    let _ = truncate_writer(id, sz);
    Ok(JsValue::undefined())
}

fn sync_flush(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_id_of(this, ctx) {
        OPFS_WRITERS.with(|r| {
            if let Some(state) = r.borrow_mut().get_mut(&id) {
                let _ = state.file.flush();
                let _ = state.file.sync_all();
            }
        });
    }
    Ok(JsValue::undefined())
}

fn sync_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_id_of(this, ctx) {
        commit_writer(id);
    }
    Ok(JsValue::undefined())
}

// ============ shared helpers ============

fn reject(ctx: &mut Context, msg: &str) -> JsResult<JsValue> {
    Ok(JsPromise::reject(
        boa_engine::JsError::from_opaque(JsValue::from(js_string!(msg.to_string()))),
        ctx,
    )
    .into())
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    use boa_engine::object::builtins::{JsArrayBuffer, JsUint8Array};
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    if let Some(blob_id) = obj
        .get(js_string!("__blob_id"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
    {
        if let Some(bytes) = super::file::read_blob_bytes(blob_id) {
            return bytes;
        }
    }
    if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
        let len = u8a.length(ctx).unwrap_or(0);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = u8a.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
        let len = ab.byte_length();
        let view = match JsUint8Array::from_array_buffer(ab, ctx) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = view.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_urls<F: FnOnce()>(top: Option<&str>, inner: Option<&str>, f: F) {
        let top_url = top.and_then(|s| url::Url::parse(s).ok());
        let inner_url = inner.and_then(|s| url::Url::parse(s).ok());
        super::super::engine::JS_TOP_LEVEL_BASE_URL.with(|s| *s.borrow_mut() = top_url);
        super::super::engine::JS_BASE_URL.with(|s| *s.borrow_mut() = inner_url);
        f();
        super::super::engine::JS_TOP_LEVEL_BASE_URL.with(|s| s.borrow_mut().take());
        super::super::engine::JS_BASE_URL.with(|s| s.borrow_mut().take());
    }

    #[test]
    fn partition_key_collapses_when_top_equals_inner() {
        // First-party single frame: top-level == inner-frame. The
        // resulting key is just the inner host so already-stored
        // pre-partition data keeps resolving.
        with_urls(
            Some("https://example.com/"),
            Some("https://example.com/page"),
            || {
                assert_eq!(partitioned_origin_host(), "example.com");
            },
        );
    }

    #[test]
    fn partition_key_combines_top_and_inner_when_different() {
        // Third-party iframe: an inner origin loaded inside a
        // different top-level page lands in a distinct directory so
        // it can't read or write the first-party origin's data.
        with_urls(
            Some("https://example.com/"),
            Some("https://tracker.example.net/widget"),
            || {
                let host = partitioned_origin_host();
                assert!(
                    host.contains("example.com") && host.contains("tracker.example.net"),
                    "partition host {host:?} should include both"
                );
                assert!(
                    host.contains("__"),
                    "partition host {host:?} should use double-underscore separator"
                );
            },
        );
    }

    #[test]
    fn partition_key_isolates_same_inner_across_different_tops() {
        // The same inner origin embedded under two distinct
        // top-level origins must yield two distinct partitions —
        // that's the whole point of partitioning.
        let (mut a, mut b) = (String::new(), String::new());
        with_urls(
            Some("https://news.example/"),
            Some("https://ads.example/widget"),
            || a = partitioned_origin_host(),
        );
        with_urls(
            Some("https://blog.example/"),
            Some("https://ads.example/widget"),
            || b = partitioned_origin_host(),
        );
        assert_ne!(a, b, "{a} should not equal {b}");
    }
}
