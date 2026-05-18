//! WebAuthn — `navigator.credentials.create()` / `.get()` with
//! passkey-shaped responses backed by `ring`'s ECDSA P-256.
//!
//! Storage: passkeys live in a thread-local registry keyed by an
//! integer credential id (also the JS-facing `id` after base64url
//! encoding). Each entry holds the PKCS#8 private key and the
//! relying-party id so `get()` can pick a matching credential when
//! the page sends an `allowCredentials` list.
//!
//! Encoded outputs:
//!   * `clientDataJSON` — the spec's CollectedClientData with `type`,
//!     `challenge` (base64url), `origin`. UTF-8 bytes go into an
//!     ArrayBuffer.
//!   * `attestationObject` — minimal CBOR { `fmt: "none"`, `attStmt:
//!     {}`, `authData: <bytes>` }, where authData is rpIdHash (32) +
//!     flags + signCount (4) + AAGUID (16) + credIdLen (2) + credId
//!     + COSE-encoded P-256 public key.
//!   * Assertion `signature` is a DER ECDSA-P256 signature over
//!     `authenticatorData || sha256(clientDataJSON)` — exactly what
//!     a real authenticator emits.
//!
//! Out of scope: cross-origin iframe checks, `attestation:
//! "direct"` (we always emit "none"), UV biometric prompts,
//! large-blob extension, FIDO U2F backward compat.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{builtins::JsArrayBuffer, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use ring::{digest, rand::SecureRandom, signature};

#[derive(Clone)]
pub struct Credential {
    /// 32-byte raw credential id we generate randomly.
    pub credential_id: Vec<u8>,
    pub rp_id: String,
    /// PKCS#8 encoding of the ECDSA P-256 private key (ring's native
    /// format).
    pub pkcs8: Vec<u8>,
    /// Cached uncompressed public point: `0x04 || X (32) || Y (32)`.
    pub public_key: Vec<u8>,
    /// Spec calls this `userHandle` — passed in at registration,
    /// echoed back on assertion.
    pub user_handle: Vec<u8>,
    pub sign_count: u32,
}

thread_local! {
    pub(crate) static CREDENTIALS: RefCell<HashMap<u32, Credential>> = RefCell::new(HashMap::new());
    pub(crate) static CRED_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
    pub(crate) static SYSTEM_RNG: RefCell<Option<std::rc::Rc<ring::rand::SystemRandom>>> =
        const { RefCell::new(None) };
}

fn rng() -> std::rc::Rc<ring::rand::SystemRandom> {
    SYSTEM_RNG.with(|s| {
        if let Some(r) = s.borrow().as_ref() {
            return r.clone();
        }
        let r = std::rc::Rc::new(ring::rand::SystemRandom::new());
        *s.borrow_mut() = Some(r.clone());
        r
    })
}

fn next_cred_id() -> u32 {
    CRED_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let create_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(credentials_create),
    )
    .build();
    let get_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(credentials_get),
    )
    .build();
    let preventsilent_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(credentials_prevent_silent_access),
    )
    .build();
    let store_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(credentials_store),
    )
    .build();
    let credentials = ObjectInitializer::new(ctx)
        .property(js_string!("create"), JsValue::from(create_fn), Attribute::READONLY)
        .property(js_string!("get"), JsValue::from(get_fn), Attribute::READONLY)
        .property(
            js_string!("preventSilentAccess"),
            JsValue::from(preventsilent_fn),
            Attribute::READONLY,
        )
        .property(js_string!("store"), JsValue::from(store_fn), Attribute::READONLY)
        .build();
    let global = ctx.global_object();
    if let Ok(nav_val) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.set(
                js_string!("credentials"),
                JsValue::from(credentials),
                false,
                ctx,
            );
        }
    }
    // Expose `PublicKeyCredential` as a global with a tiny static
    // surface that feature-detection paths hit.
    let isuvpaa = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(is_uvpaa),
    )
    .build();
    let isccp = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(is_conditional_mediation_available),
    )
    .build();
    let pkc_ctor = ObjectInitializer::new(ctx)
        .property(
            js_string!("isUserVerifyingPlatformAuthenticatorAvailable"),
            JsValue::from(isuvpaa),
            Attribute::READONLY,
        )
        .property(
            js_string!("isConditionalMediationAvailable"),
            JsValue::from(isccp),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("PublicKeyCredential"),
        pkc_ctor,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn is_uvpaa(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn is_conditional_mediation_available(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn credentials_prevent_silent_access(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn credentials_store(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Identity passthrough — pages call `credentials.store(cred)` to
    // re-confirm the system's persistence; we hand the credential
    // back unchanged.
    let v = args.first().cloned().unwrap_or(JsValue::undefined());
    Ok(JsPromise::resolve(v, ctx).into())
}

// ============ create() ============

fn credentials_create(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(opts_obj) = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("publicKey"), ctx).ok())
        .and_then(|v| v.as_object().cloned())
    else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "credentials.create: missing publicKey options"
            ))),
            ctx,
        )
        .into());
    };
    // Extract rp.id (the relying-party origin), user.id (Uint8Array
    // user handle), and challenge (Uint8Array) from the descriptor.
    let rp_id = opts_obj
        .get(js_string!("rp"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .and_then(|o| o.get(js_string!("id"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| current_origin(ctx).host_string());
    let user_handle = opts_obj
        .get(js_string!("user"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .and_then(|o| o.get(js_string!("id"), ctx).ok())
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    let challenge = opts_obj
        .get(js_string!("challenge"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();

    let pkcs8 = match signature::EcdsaKeyPair::generate_pkcs8(
        &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        rng().as_ref(),
    ) {
        Ok(doc) => doc.as_ref().to_vec(),
        Err(_) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                    "credentials.create: keygen failed"
                ))),
                ctx,
            )
            .into());
        }
    };
    let kp = match signature::EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &pkcs8,
        rng().as_ref(),
    ) {
        Ok(k) => k,
        Err(_) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                    "credentials.create: key parse failed"
                ))),
                ctx,
            )
            .into());
        }
    };
    let public_point = signature::KeyPair::public_key(&kp).as_ref().to_vec();
    // 32-byte random credential id.
    let mut credential_id = vec![0u8; 32];
    if rng().fill(&mut credential_id).is_err() {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "credentials.create: rng failure"
            ))),
            ctx,
        )
        .into());
    }
    let cred = Credential {
        credential_id: credential_id.clone(),
        rp_id: rp_id.clone(),
        pkcs8,
        public_key: public_point.clone(),
        user_handle: user_handle.clone(),
        sign_count: 0,
    };
    let id = next_cred_id();
    CREDENTIALS.with(|r| r.borrow_mut().insert(id, cred));

    // Build authData + attestationObject.
    let rp_id_hash = sha256(rp_id.as_bytes());
    let aaguid = [0u8; 16];
    let cose_pub = cose_p256_public_key(&public_point);
    let auth_data = build_auth_data(
        &rp_id_hash,
        AUTH_DATA_FLAG_UP | AUTH_DATA_FLAG_AT,
        0,
        Some((&aaguid, &credential_id, &cose_pub)),
    );
    let att_obj = build_attestation_object(&auth_data);

    let origin = current_origin(ctx).ascii_serialization();
    let client_data = build_client_data_json("webauthn.create", &challenge, &origin);
    let credential_obj = build_credential_object(
        ctx,
        &credential_id,
        Some(&att_obj),
        Some(&auth_data),
        None,
        None,
        &client_data,
    );
    Ok(JsPromise::resolve(credential_obj, ctx).into())
}

// ============ get() ============

fn credentials_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(opts_obj) = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("publicKey"), ctx).ok())
        .and_then(|v| v.as_object().cloned())
    else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "credentials.get: missing publicKey options"
            ))),
            ctx,
        )
        .into());
    };
    let rp_id = opts_obj
        .get(js_string!("rpId"), ctx)
        .ok()
        .and_then(|v| {
            if v.is_undefined() || v.is_null() {
                None
            } else {
                v.to_string(ctx).ok().map(|s| s.to_std_string_escaped())
            }
        })
        .unwrap_or_else(|| current_origin(ctx).host_string());
    let challenge = opts_obj
        .get(js_string!("challenge"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    // allowCredentials: list of { id, type, transports }. We match
    // any whose `id` equals one of our stored credential ids.
    let allowed_ids: Vec<Vec<u8>> = opts_obj
        .get(js_string!("allowCredentials"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|arr| {
            let len = arr
                .get(js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .unwrap_or(0);
            (0..len)
                .filter_map(|i| {
                    arr.get(i, ctx)
                        .ok()
                        .and_then(|item| item.as_object().cloned())
                        .and_then(|o| o.get(js_string!("id"), ctx).ok())
                        .map(|v| read_bytes(&v, ctx))
                })
                .collect()
        })
        .unwrap_or_default();

    // Pick the first stored credential that matches rp + allowList.
    let pick = CREDENTIALS.with(|r| -> Option<(u32, Credential)> {
        for (id, cred) in r.borrow().iter() {
            if cred.rp_id != rp_id {
                continue;
            }
            if !allowed_ids.is_empty()
                && !allowed_ids.iter().any(|c| c == &cred.credential_id)
            {
                continue;
            }
            return Some((*id, cred.clone()));
        }
        None
    });
    let Some((cred_id, mut cred)) = pick else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "credentials.get: no matching credential"
            ))),
            ctx,
        )
        .into());
    };
    cred.sign_count = cred.sign_count.saturating_add(1);
    CREDENTIALS.with(|r| {
        if let Some(c) = r.borrow_mut().get_mut(&cred_id) {
            c.sign_count = cred.sign_count;
        }
    });

    let rp_id_hash = sha256(cred.rp_id.as_bytes());
    let auth_data = build_auth_data(
        &rp_id_hash,
        AUTH_DATA_FLAG_UP,
        cred.sign_count,
        None,
    );
    let origin = current_origin(ctx).ascii_serialization();
    let client_data = build_client_data_json("webauthn.get", &challenge, &origin);
    let client_data_hash = sha256(&client_data);
    // Sign authenticatorData || clientDataHash with the credential's
    // private key.
    let kp = match signature::EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &cred.pkcs8,
        rng().as_ref(),
    ) {
        Ok(k) => k,
        Err(_) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                    "credentials.get: key parse failed"
                ))),
                ctx,
            )
            .into());
        }
    };
    let mut to_sign = Vec::with_capacity(auth_data.len() + client_data_hash.len());
    to_sign.extend_from_slice(&auth_data);
    to_sign.extend_from_slice(&client_data_hash);
    // FIXED signing gives raw 64-byte (r||s). Real WebAuthn wants DER.
    let sig_raw = match kp.sign(rng().as_ref(), &to_sign) {
        Ok(s) => s.as_ref().to_vec(),
        Err(_) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                    "credentials.get: signing failed"
                ))),
                ctx,
            )
            .into());
        }
    };
    let signature_der = ecdsa_fixed_to_der(&sig_raw);
    let credential_obj = build_credential_object(
        ctx,
        &cred.credential_id,
        None,
        Some(&auth_data),
        Some(&signature_der),
        Some(&cred.user_handle),
        &client_data,
    );
    Ok(JsPromise::resolve(credential_obj, ctx).into())
}

// ============ helpers ============

fn current_origin(ctx: &mut Context) -> url::Origin {
    let url = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    url.map(|u| u.origin())
        .unwrap_or_else(|| url::Origin::new_opaque())
}

trait OriginExt {
    fn host_string(&self) -> String;
}

impl OriginExt for url::Origin {
    fn host_string(&self) -> String {
        // Tuple origins serialize to "scheme://host[:port]"; we want
        // just `host` for rpId.
        match self {
            url::Origin::Tuple(_, host, _) => host.to_string(),
            url::Origin::Opaque(_) => "localhost".to_string(),
        }
    }
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    use boa_engine::object::builtins::JsUint8Array;
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
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
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        if let Ok(v) = obj.get(i, ctx) {
            if let Ok(n) = v.to_u32(ctx) {
                out.push(n as u8);
            }
        }
    }
    out
}

fn sha256(data: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

const AUTH_DATA_FLAG_UP: u8 = 0x01;
const AUTH_DATA_FLAG_AT: u8 = 0x40;

fn build_auth_data(
    rp_id_hash: &[u8],
    flags: u8,
    sign_count: u32,
    attested: Option<(&[u8; 16], &[u8], &[u8])>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(rp_id_hash);
    out.push(flags);
    out.extend_from_slice(&sign_count.to_be_bytes());
    if let Some((aaguid, cred_id, cose_pub)) = attested {
        out.extend_from_slice(aaguid);
        out.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        out.extend_from_slice(cred_id);
        out.extend_from_slice(cose_pub);
    }
    out
}

/// Hand-rolled CBOR for the COSE_Key (-7 / EC2 / P-256). Output is
/// the canonical CBOR encoding most authenticators emit.
fn cose_p256_public_key(uncompressed_point: &[u8]) -> Vec<u8> {
    // Strip the leading 0x04 from ring's uncompressed point; split
    // X (32) + Y (32).
    let bytes = if uncompressed_point.len() == 65 && uncompressed_point[0] == 0x04 {
        &uncompressed_point[1..]
    } else {
        uncompressed_point
    };
    if bytes.len() < 64 {
        return Vec::new();
    }
    let x = &bytes[..32];
    let y = &bytes[32..64];
    // Map with 5 entries: { 1: 2, 3: -7, -1: 1, -2: X, -3: Y }
    // CBOR major type 5 (map), length 5.
    let mut out = Vec::with_capacity(96);
    out.push(0xa5);
    // key 1 (kty), value 2 (EC2)
    out.extend_from_slice(&[0x01, 0x02]);
    // key 3 (alg), value -7 (ES256) → CBOR negative int: 0x26
    out.extend_from_slice(&[0x03, 0x26]);
    // key -1 (crv) → CBOR negative int 0x20, value 1 (P-256)
    out.extend_from_slice(&[0x20, 0x01]);
    // key -2 → 0x21, value byte string of length 32
    out.push(0x21);
    out.push(0x58);
    out.push(32);
    out.extend_from_slice(x);
    // key -3 → 0x22, value byte string of length 32
    out.push(0x22);
    out.push(0x58);
    out.push(32);
    out.extend_from_slice(y);
    out
}

fn build_attestation_object(auth_data: &[u8]) -> Vec<u8> {
    // CBOR map of 3 entries: { fmt: "none", attStmt: {}, authData: <bytes> }
    let mut out = Vec::with_capacity(auth_data.len() + 32);
    out.push(0xa3); // map(3)
    // "fmt" → "none"
    cbor_text(&mut out, "fmt");
    cbor_text(&mut out, "none");
    // "attStmt" → {}
    cbor_text(&mut out, "attStmt");
    out.push(0xa0); // empty map
    // "authData" → bytes
    cbor_text(&mut out, "authData");
    cbor_bytes(&mut out, auth_data);
    out
}

fn cbor_text(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len();
    if n < 24 {
        out.push(0x60 | n as u8);
    } else if n <= 0xff {
        out.push(0x78);
        out.push(n as u8);
    } else {
        out.push(0x79);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn cbor_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let n = bytes.len();
    if n < 24 {
        out.push(0x40 | n as u8);
    } else if n <= 0xff {
        out.push(0x58);
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(0x59);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x5a);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn build_client_data_json(ty: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
    let challenge_b64 = base64url_encode(challenge);
    format!(
        r#"{{"type":"{ty}","challenge":"{challenge_b64}","origin":"{origin}","crossOrigin":false}}"#
    )
    .into_bytes()
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
    }
    // WebAuthn uses unpadded base64url, no trailing '='.
    out
}

/// Convert a fixed-length 64-byte ECDSA P-256 signature (r||s) into
/// the DER form WebAuthn assertions surface.
fn ecdsa_fixed_to_der(raw: &[u8]) -> Vec<u8> {
    if raw.len() != 64 {
        return raw.to_vec();
    }
    let r = strip_leading_zeros(&raw[..32]);
    let s = strip_leading_zeros(&raw[32..]);
    let r = pad_for_der(&r);
    let s = pad_for_der(&s);
    let mut body = Vec::new();
    body.push(0x02);
    body.push(r.len() as u8);
    body.extend_from_slice(&r);
    body.push(0x02);
    body.push(s.len() as u8);
    body.extend_from_slice(&s);
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(0x30);
    out.push(body.len() as u8);
    out.extend_from_slice(&body);
    out
}

fn strip_leading_zeros(bytes: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i + 1 < bytes.len() && bytes[i] == 0x00 {
        i += 1;
    }
    bytes[i..].to_vec()
}

fn pad_for_der(bytes: &[u8]) -> Vec<u8> {
    // DER INTEGER must be positive; prepend 0x00 if high bit set.
    if bytes.first().map(|b| b & 0x80 != 0).unwrap_or(false) {
        let mut out = vec![0x00];
        out.extend_from_slice(bytes);
        out
    } else {
        bytes.to_vec()
    }
}

// ============ JS object construction ============

#[allow(clippy::too_many_arguments)]
fn build_credential_object(
    ctx: &mut Context,
    credential_id: &[u8],
    attestation_object: Option<&[u8]>,
    authenticator_data: Option<&[u8]>,
    signature_bytes: Option<&[u8]>,
    user_handle: Option<&[u8]>,
    client_data_json: &[u8],
) -> JsValue {
    let id_b64 = base64url_encode(credential_id);
    let raw_id = ab_from_bytes(ctx, credential_id);
    let response = build_response_object(
        ctx,
        attestation_object,
        authenticator_data,
        signature_bytes,
        user_handle,
        client_data_json,
    );
    let realm = ctx.realm().clone();
    let get_ext = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(empty_obj),
    )
    .build();
    ObjectInitializer::new(ctx)
        .property(
            js_string!("id"),
            JsValue::from(js_string!(id_b64)),
            Attribute::READONLY,
        )
        .property(js_string!("rawId"), raw_id, Attribute::READONLY)
        .property(
            js_string!("type"),
            JsValue::from(js_string!("public-key")),
            Attribute::READONLY,
        )
        .property(
            js_string!("authenticatorAttachment"),
            JsValue::from(js_string!("platform")),
            Attribute::READONLY,
        )
        .property(js_string!("response"), response, Attribute::READONLY)
        .property(
            js_string!("getClientExtensionResults"),
            JsValue::from(get_ext),
            Attribute::READONLY,
        )
        .build()
        .into()
}

fn build_response_object(
    ctx: &mut Context,
    attestation_object: Option<&[u8]>,
    authenticator_data: Option<&[u8]>,
    signature_bytes: Option<&[u8]>,
    user_handle: Option<&[u8]>,
    client_data_json: &[u8],
) -> JsValue {
    // Build every JsArrayBuffer up front so we hold the values
    // owned, then assemble the ObjectInitializer in one go without
    // re-borrowing ctx.
    let cd_ab = ab_from_bytes(ctx, client_data_json);
    let att_ab = attestation_object.map(|att| ab_from_bytes(ctx, att));
    let ad_ab = authenticator_data.map(|ad| ab_from_bytes(ctx, ad));
    let sig_ab = signature_bytes.map(|s| ab_from_bytes(ctx, s));
    let uh_val = user_handle.map(|uh| {
        if uh.is_empty() {
            JsValue::null()
        } else {
            ab_from_bytes(ctx, uh)
        }
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!("clientDataJSON"), cd_ab, Attribute::READONLY);
    if let Some(att) = att_ab {
        b.property(js_string!("attestationObject"), att, Attribute::READONLY);
    }
    if let Some(ad) = ad_ab {
        b.property(js_string!("authenticatorData"), ad, Attribute::READONLY);
    }
    if let Some(sig) = sig_ab {
        b.property(js_string!("signature"), sig, Attribute::READONLY);
    }
    if let Some(uh) = uh_val {
        b.property(js_string!("userHandle"), uh, Attribute::READONLY);
    }
    JsValue::from(b.build())
}

fn ab_from_bytes(ctx: &mut Context, bytes: &[u8]) -> JsValue {
    match JsArrayBuffer::from_byte_block(bytes.to_vec(), ctx) {
        Ok(ab) => JsValue::from(ab),
        Err(_) => JsValue::null(),
    }
}

fn empty_obj(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(ctx).build().into())
}
