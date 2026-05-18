//! `crypto` global + `crypto.subtle` async surface backed by `ring`.
//!
//! Synchronous (`crypto.*`):
//!   * `getRandomValues(typedArray)` — fills with cryptographically
//!     secure random bytes, returns the same view (per spec).
//!   * `randomUUID()` — RFC 4122 v4 UUID string.
//!
//! Async (`crypto.subtle.*`) — all return Promises that resolve
//! synchronously (we don't have a worker pool, but Boa's Promise
//! machinery still wraps the value correctly for `await`):
//!   * `digest(alg, data)` — SHA-1 / SHA-256 / SHA-384 / SHA-512.
//!   * `generateKey(alg, extractable, usages)` — AES-GCM,
//!     HMAC (any SHA variant) — symmetric only.
//!   * `importKey(format, key, alg, extractable, usages)` — `raw`
//!     format only.
//!   * `exportKey(format, key)` — `raw` only, fails on non-extractable.
//!   * `sign(alg, key, data)` / `verify(alg, key, sig, data)` — HMAC.
//!   * `encrypt(alg, key, data)` / `decrypt(alg, key, data)` —
//!     AES-GCM (12-byte IV, 128-bit tag suffix).
//!
//! Out of scope: RSA / ECDSA / ECDH (need RsaKeyPair / EcdsaKeyPair
//! plumbing on the ring side); PBKDF2 / HKDF (need ring::pbkdf2);
//! key formats beyond `raw`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArrayBuffer, JsPromise, JsUint8Array},
        FunctionObjectBuilder, ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use ring::{aead, digest, hmac, rand::SecureRandom, rand::SystemRandom};

#[derive(Clone)]
pub struct CryptoKeyEntry {
    pub algorithm: String,
    /// SHA-1 / SHA-256 / SHA-384 / SHA-512 — relevant for HMAC.
    pub hash: Option<String>,
    pub extractable: bool,
    pub usages: Vec<String>,
    pub raw: Vec<u8>,
}

pub type KeyRegistry = Rc<RefCell<HashMap<u32, CryptoKeyEntry>>>;

thread_local! {
    pub(crate) static KEY_REGISTRY: RefCell<KeyRegistry> =
        RefCell::new(Rc::new(RefCell::new(HashMap::new())));
    pub(crate) static KEY_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
    pub(crate) static SYSTEM_RNG: RefCell<Option<Rc<SystemRandom>>> =
        const { RefCell::new(None) };
}

fn rng() -> Rc<SystemRandom> {
    SYSTEM_RNG.with(|slot| {
        if let Some(r) = slot.borrow().as_ref() {
            return r.clone();
        }
        let r = Rc::new(SystemRandom::new());
        *slot.borrow_mut() = Some(r.clone());
        r
    })
}

fn next_key_id() -> u32 {
    KEY_NEXT_ID.with(|slot| {
        let mut s = slot.borrow_mut();
        let id = *s;
        *s = s.wrapping_add(1);
        id
    })
}

const CRYPTO_KEY_ID: &str = "__crypto_key_id";

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let get_random_values = FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(get_random_values),
    )
    .build();
    let random_uuid =
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(random_uuid)).build();
    let subtle = build_subtle(ctx);
    let crypto = ObjectInitializer::new(ctx)
        .property(
            js_string!("getRandomValues"),
            JsValue::from(get_random_values),
            Attribute::READONLY,
        )
        .property(
            js_string!("randomUUID"),
            JsValue::from(random_uuid),
            Attribute::READONLY,
        )
        .property(js_string!("subtle"), subtle, Attribute::READONLY)
        .build();
    let _ = ctx.register_global_property(
        js_string!("crypto"),
        crypto,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn build_subtle(ctx: &mut Context) -> JsValue {
    let realm = ctx.realm().clone();
    let mut methods: Vec<(&str, NativeFunction)> = Vec::new();
    methods.push(("digest", NativeFunction::from_fn_ptr(subtle_digest)));
    methods.push(("generateKey", NativeFunction::from_fn_ptr(subtle_generate_key)));
    methods.push(("importKey", NativeFunction::from_fn_ptr(subtle_import_key)));
    methods.push(("exportKey", NativeFunction::from_fn_ptr(subtle_export_key)));
    methods.push(("sign", NativeFunction::from_fn_ptr(subtle_sign)));
    methods.push(("verify", NativeFunction::from_fn_ptr(subtle_verify)));
    methods.push(("encrypt", NativeFunction::from_fn_ptr(subtle_encrypt)));
    methods.push(("decrypt", NativeFunction::from_fn_ptr(subtle_decrypt)));

    let mut entries: Vec<(&str, JsValue)> = Vec::with_capacity(methods.len());
    for (name, f) in methods {
        let func = FunctionObjectBuilder::new(&realm, f).build();
        entries.push((name, JsValue::from(func)));
    }
    let mut b = ObjectInitializer::new(ctx);
    for (name, value) in entries {
        b.property(js_string!(name), value, Attribute::READONLY);
    }
    JsValue::from(b.build())
}

// ============ synchronous crypto.* ============

fn get_random_values(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arr_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(obj) = arr_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0) as usize;
    if len > 65_536 {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getRandomValues: array too large (max 65536 bytes)")
            .into());
    }
    let bytes_per_elem = obj
        .get(js_string!("BYTES_PER_ELEMENT"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(1);
    let total = len.saturating_mul(bytes_per_elem as usize);
    let mut bytes = vec![0u8; total];
    if rng().fill(&mut bytes).is_err() {
        return Err(boa_engine::JsNativeError::error()
            .with_message("getRandomValues: rng failure")
            .into());
    }
    // Write back element-by-element to honour the array's element
    // width interpretation.
    for i in 0..len {
        let off = i * bytes_per_elem as usize;
        let value = match bytes_per_elem {
            1 => bytes[off] as u32 as f64,
            2 => u16::from_le_bytes([bytes[off], bytes[off + 1]]) as f64,
            4 => u32::from_le_bytes([
                bytes[off],
                bytes[off + 1],
                bytes[off + 2],
                bytes[off + 3],
            ]) as f64,
            _ => 0.0,
        };
        let _ = obj.set(i as u32, JsValue::from(value), false, ctx);
    }
    Ok(arr_val.clone())
}

fn random_uuid(_: &JsValue, _: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let mut bytes = [0u8; 16];
    if rng().fill(&mut bytes).is_err() {
        return Err(boa_engine::JsNativeError::error()
            .with_message("randomUUID: rng failure")
            .into());
    }
    // Stamp version 4 + variant bits per RFC 4122 §4.4.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let s = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    );
    Ok(JsValue::from(js_string!(s)))
}

// ============ helpers ============

fn algorithm_name(val: &JsValue, ctx: &mut Context) -> Option<String> {
    if val.is_string() {
        return val.to_string(ctx).ok().map(|s| s.to_std_string_escaped());
    }
    let obj = val.as_object()?;
    let name = obj.get(js_string!("name"), ctx).ok()?;
    name.to_string(ctx).ok().map(|s| s.to_std_string_escaped())
}

fn algorithm_hash(val: &JsValue, ctx: &mut Context) -> Option<String> {
    let obj = val.as_object()?;
    let hash = obj.get(js_string!("hash"), ctx).ok()?;
    if hash.is_string() {
        return hash.to_string(ctx).ok().map(|s| s.to_std_string_escaped());
    }
    if let Some(hash_obj) = hash.as_object() {
        let name = hash_obj.get(js_string!("name"), ctx).ok()?;
        return name.to_string(ctx).ok().map(|s| s.to_std_string_escaped());
    }
    None
}

fn algorithm_iv(val: &JsValue, ctx: &mut Context) -> Option<Vec<u8>> {
    let obj = val.as_object()?;
    let iv = obj.get(js_string!("iv"), ctx).ok()?;
    read_bytes(&iv, ctx)
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Option<Vec<u8>> {
    let obj = val.as_object()?;
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
        let len = ab.byte_length();
        let mut out = vec![0u8; len];
        if !out.is_empty() {
            if let Ok(view) = JsUint8Array::from_array_buffer(ab, ctx) {
                for i in 0..len {
                    let v = view.at(i as i64, ctx).ok()?;
                    out[i] = v.to_u32(ctx).ok()? as u8;
                }
                return Some(out);
            }
        }
        return Some(out);
    }
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

fn bytes_to_array_buffer(bytes: Vec<u8>, ctx: &mut Context) -> JsResult<JsValue> {
    let ab = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    Ok(JsValue::from(ab))
}

fn wrap_promise<F>(ctx: &mut Context, work: F) -> JsResult<JsValue>
where
    F: FnOnce(&mut Context) -> Result<JsValue, String>,
{
    match work(ctx) {
        Ok(v) => Ok(JsPromise::resolve(v, ctx).into()),
        Err(msg) => Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(msg))),
            ctx,
        )
        .into()),
    }
}

fn store_key(entry: CryptoKeyEntry) -> u32 {
    let id = next_key_id();
    KEY_REGISTRY.with(|slot| {
        let rc = slot.borrow().clone();
        rc.borrow_mut().insert(id, entry);
    });
    id
}

fn load_key(val: &JsValue, ctx: &mut Context) -> Option<CryptoKeyEntry> {
    let obj = val.as_object()?;
    let id = obj
        .get(js_string!(CRYPTO_KEY_ID), ctx)
        .ok()?
        .to_u32(ctx)
        .ok()?;
    KEY_REGISTRY.with(|slot| {
        let rc = slot.borrow().clone();
        let result = rc.borrow().get(&id).cloned();
        result
    })
}

fn build_key_object(ctx: &mut Context, id: u32, entry: &CryptoKeyEntry, kind: &str) -> JsValue {
    let usages = boa_engine::object::builtins::JsArray::new(ctx);
    for u in &entry.usages {
        let _ = usages.push(JsValue::from(js_string!(u.clone())), ctx);
    }
    let mut alg = ObjectInitializer::new(ctx);
    alg.property(
        js_string!("name"),
        JsValue::from(js_string!(entry.algorithm.clone())),
        Attribute::READONLY,
    );
    if let Some(h) = &entry.hash {
        alg.property(
            js_string!("hash"),
            JsValue::from(js_string!(h.clone())),
            Attribute::READONLY,
        );
    }
    if entry.algorithm.starts_with("AES") || entry.algorithm.starts_with("HMAC") {
        alg.property(
            js_string!("length"),
            JsValue::from((entry.raw.len() * 8) as u32),
            Attribute::READONLY,
        );
    }
    let alg_obj = alg.build();
    let obj = ObjectInitializer::new(ctx)
        .property(
            js_string!(CRYPTO_KEY_ID),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!(kind.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("extractable"),
            JsValue::from(entry.extractable),
            Attribute::READONLY,
        )
        .property(
            js_string!("usages"),
            JsValue::from(usages),
            Attribute::READONLY,
        )
        .property(js_string!("algorithm"), JsValue::from(alg_obj), Attribute::READONLY)
        .build();
    JsValue::from(obj)
}

fn digest_algorithm(name: &str) -> Option<&'static digest::Algorithm> {
    match name.to_ascii_uppercase().as_str() {
        "SHA-1" => Some(&digest::SHA1_FOR_LEGACY_USE_ONLY),
        "SHA-256" => Some(&digest::SHA256),
        "SHA-384" => Some(&digest::SHA384),
        "SHA-512" => Some(&digest::SHA512),
        _ => None,
    }
}

fn hmac_algorithm(hash: &str) -> Option<hmac::Algorithm> {
    match hash.to_ascii_uppercase().as_str() {
        "SHA-1" => Some(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY),
        "SHA-256" => Some(hmac::HMAC_SHA256),
        "SHA-384" => Some(hmac::HMAC_SHA384),
        "SHA-512" => Some(hmac::HMAC_SHA512),
        _ => None,
    }
}

fn aead_algorithm(name: &str, key_len: usize) -> Option<&'static aead::Algorithm> {
    match (name.to_ascii_uppercase().as_str(), key_len) {
        ("AES-GCM", 16) => Some(&aead::AES_128_GCM),
        ("AES-GCM", 32) => Some(&aead::AES_256_GCM),
        _ => None,
    }
}

// ============ subtle.* implementations ============

fn subtle_digest(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let data_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        let name = algorithm_name(&alg_val, ctx)
            .ok_or_else(|| "digest: missing algorithm".to_string())?;
        let algo = digest_algorithm(&name)
            .ok_or_else(|| format!("digest: unsupported algorithm {name}"))?;
        let bytes = read_bytes(&data_val, ctx)
            .ok_or_else(|| "digest: invalid data".to_string())?;
        let out = digest::digest(algo, &bytes).as_ref().to_vec();
        bytes_to_array_buffer(out, ctx).map_err(|e| format!("digest: {e:?}"))
    })
}

fn subtle_generate_key(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let extractable = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
    let usages = read_usages(args.get(2), ctx);
    wrap_promise(ctx, move |ctx| {
        let name = algorithm_name(&alg_val, ctx)
            .ok_or_else(|| "generateKey: missing algorithm".to_string())?;
        let alg_obj = alg_val.as_object();
        let length_bits = alg_obj
            .as_ref()
            .and_then(|o| o.get(js_string!("length"), ctx).ok())
            .and_then(|v| v.to_u32(ctx).ok())
            .unwrap_or(0);
        let hash = algorithm_hash(&alg_val, ctx);
        let key_bytes = match name.to_ascii_uppercase().as_str() {
            "AES-GCM" | "AES-CBC" | "AES-CTR" => {
                let bits = if length_bits == 0 { 256 } else { length_bits };
                if !matches!(bits, 128 | 192 | 256) {
                    return Err(format!("generateKey: AES key length {bits} not supported"));
                }
                let mut bytes = vec![0u8; (bits / 8) as usize];
                rng()
                    .fill(&mut bytes)
                    .map_err(|_| "generateKey: rng failure".to_string())?;
                bytes
            }
            "HMAC" => {
                let h = hash
                    .clone()
                    .ok_or_else(|| "generateKey: HMAC requires hash".to_string())?;
                let algo = hmac_algorithm(&h)
                    .ok_or_else(|| format!("generateKey: unsupported HMAC hash {h}"))?;
                let mut bytes = vec![0u8; algo.digest_algorithm().output_len()];
                rng()
                    .fill(&mut bytes)
                    .map_err(|_| "generateKey: rng failure".to_string())?;
                bytes
            }
            other => {
                return Err(format!("generateKey: algorithm '{other}' unsupported"))
            }
        };
        let entry = CryptoKeyEntry {
            algorithm: name.to_uppercase(),
            hash,
            extractable,
            usages: usages.clone(),
            raw: key_bytes,
        };
        let id = store_key(entry.clone());
        Ok(build_key_object(ctx, id, &entry, "secret"))
    })
}

fn subtle_import_key(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let format = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let alg_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
    let extractable = args.get(3).map(|v| v.to_boolean()).unwrap_or(false);
    let usages = read_usages(args.get(4), ctx);
    wrap_promise(ctx, move |ctx| {
        if format != "raw" {
            return Err(format!("importKey: format '{format}' not supported"));
        }
        let raw = read_bytes(&key_val, ctx)
            .ok_or_else(|| "importKey: invalid key data".to_string())?;
        let name = algorithm_name(&alg_val, ctx)
            .ok_or_else(|| "importKey: missing algorithm".to_string())?;
        let hash = algorithm_hash(&alg_val, ctx);
        let entry = CryptoKeyEntry {
            algorithm: name.to_uppercase(),
            hash,
            extractable,
            usages: usages.clone(),
            raw,
        };
        let id = store_key(entry.clone());
        Ok(build_key_object(ctx, id, &entry, "secret"))
    })
}

fn subtle_export_key(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let format = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        if format != "raw" {
            return Err(format!("exportKey: format '{format}' not supported"));
        }
        let entry =
            load_key(&key_val, ctx).ok_or_else(|| "exportKey: invalid key".to_string())?;
        if !entry.extractable {
            return Err("exportKey: key is not extractable".to_string());
        }
        bytes_to_array_buffer(entry.raw, ctx).map_err(|e| format!("exportKey: {e:?}"))
    })
}

fn subtle_sign(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let data_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        let name = algorithm_name(&alg_val, ctx)
            .ok_or_else(|| "sign: missing algorithm".to_string())?;
        if name.to_ascii_uppercase() != "HMAC" {
            return Err(format!("sign: algorithm '{name}' unsupported"));
        }
        let entry =
            load_key(&key_val, ctx).ok_or_else(|| "sign: invalid key".to_string())?;
        let hash = entry
            .hash
            .clone()
            .ok_or_else(|| "sign: key has no hash".to_string())?;
        let algo = hmac_algorithm(&hash)
            .ok_or_else(|| format!("sign: unsupported HMAC hash {hash}"))?;
        let data = read_bytes(&data_val, ctx)
            .ok_or_else(|| "sign: invalid data".to_string())?;
        let key = hmac::Key::new(algo, &entry.raw);
        let tag = hmac::sign(&key, &data);
        bytes_to_array_buffer(tag.as_ref().to_vec(), ctx).map_err(|e| format!("sign: {e:?}"))
    })
}

fn subtle_verify(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let sig_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
    let data_val = args.get(3).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        let name = algorithm_name(&alg_val, ctx)
            .ok_or_else(|| "verify: missing algorithm".to_string())?;
        if name.to_ascii_uppercase() != "HMAC" {
            return Err(format!("verify: algorithm '{name}' unsupported"));
        }
        let entry =
            load_key(&key_val, ctx).ok_or_else(|| "verify: invalid key".to_string())?;
        let hash = entry
            .hash
            .clone()
            .ok_or_else(|| "verify: key has no hash".to_string())?;
        let algo = hmac_algorithm(&hash)
            .ok_or_else(|| format!("verify: unsupported HMAC hash {hash}"))?;
        let sig = read_bytes(&sig_val, ctx)
            .ok_or_else(|| "verify: invalid signature".to_string())?;
        let data = read_bytes(&data_val, ctx)
            .ok_or_else(|| "verify: invalid data".to_string())?;
        let key = hmac::Key::new(algo, &entry.raw);
        Ok(JsValue::from(hmac::verify(&key, &data, &sig).is_ok()))
    })
}

fn subtle_encrypt(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let data_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        aead_op(true, &alg_val, &key_val, &data_val, ctx)
    })
}

fn subtle_decrypt(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let key_val = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let data_val = args.get(2).cloned().unwrap_or(JsValue::undefined());
    wrap_promise(ctx, move |ctx| {
        aead_op(false, &alg_val, &key_val, &data_val, ctx)
    })
}

fn aead_op(
    seal: bool,
    alg_val: &JsValue,
    key_val: &JsValue,
    data_val: &JsValue,
    ctx: &mut Context,
) -> Result<JsValue, String> {
    let name =
        algorithm_name(alg_val, ctx).ok_or_else(|| "AEAD: missing algorithm".to_string())?;
    let entry =
        load_key(key_val, ctx).ok_or_else(|| "AEAD: invalid key".to_string())?;
    let algo = aead_algorithm(&name, entry.raw.len())
        .ok_or_else(|| format!("AEAD: {name} with {}-byte key unsupported", entry.raw.len()))?;
    let iv =
        algorithm_iv(alg_val, ctx).ok_or_else(|| "AEAD: missing iv".to_string())?;
    if iv.len() != 12 {
        return Err(format!("AEAD: iv must be 12 bytes, got {}", iv.len()));
    }
    let mut iv_array = [0u8; 12];
    iv_array.copy_from_slice(&iv);
    let nonce = aead::Nonce::assume_unique_for_key(iv_array);
    let unbound = aead::UnboundKey::new(algo, &entry.raw)
        .map_err(|_| "AEAD: invalid key length".to_string())?;
    let key = aead::LessSafeKey::new(unbound);
    let aad = aead::Aad::empty();
    let mut buf =
        read_bytes(data_val, ctx).ok_or_else(|| "AEAD: invalid data".to_string())?;
    if seal {
        key.seal_in_place_append_tag(nonce, aad, &mut buf)
            .map_err(|_| "AEAD: seal failed".to_string())?;
        bytes_to_array_buffer(buf, ctx).map_err(|e| format!("AEAD: {e:?}"))
    } else {
        let plain = key
            .open_in_place(nonce, aad, &mut buf)
            .map_err(|_| "AEAD: open failed".to_string())?;
        let out = plain.to_vec();
        bytes_to_array_buffer(out, ctx).map_err(|e| format!("AEAD: {e:?}"))
    }
}

fn read_usages(val: Option<&JsValue>, ctx: &mut Context) -> Vec<String> {
    let Some(v) = val else {
        return Vec::new();
    };
    let Some(obj) = v.as_object() else {
        return Vec::new();
    };
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let v = obj.get(i, ctx).ok();
        let s = v.and_then(|v| v.to_string(ctx).ok()).map(|s| s.to_std_string_escaped());
        if let Some(s) = s {
            out.push(s);
        }
    }
    out
}
