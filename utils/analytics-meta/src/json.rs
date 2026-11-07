//! `JsonMtd`: a schema-tagged JSON document stored in `GstAnalyticsRelationMeta`.
//!
//! The data layout is `{u32 schema_len; u32 json_len; u8 bytes[]}` in native
//! byte order, followed by the schema and the JSON text in UTF-8, without NUL
//! terminators. The type has no transform or clear function, so upstream's
//! relation-meta transform copies it unchanged.
//!
//! `x-json` v1 has no callbacks *by contract*. Every library adopts one impl
//! address, so the winner's callbacks would run for every library's
//! documents: adding or changing a callback, like changing the layout, needs
//! the impl name `x-json-2` and the slot key `.v2`, together.
//!
//! Upstream identifies an Mtd type by the address of its `GstAnalyticsMtdImpl`,
//! and every plugin `cdylib` linking this crate carries its own copy of that
//! static. To make documents written by one plugin visible to another, the
//! first library to touch the type installs its static in a process-wide slot
//! (qdata on the default `GstRegistry`) with a compare-and-swap; every library
//! then uses the winner. Plugins are never unloaded (`gst_plugin_load_file`
//! makes modules resident), so the winner stays valid. Correctness rests on
//! `g_object_replace_qdata` being an atomic compare-and-exchange under
//! `GObject`'s datalist lock; the race test is a regression check, not a proof.

#![expect(
    unsafe_code,
    reason = "implements an Mtd type and the registry slot over the C API"
)]

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::glib::{
    self, gobject_ffi,
    prelude::ObjectType as _,
    translate::{IntoGlib as _, UnsafeFrom as _},
};
use gst_analytics::{AnalyticsMtd, AnalyticsMtdRef, AnalyticsRelationMeta, ffi};
use serde::Serialize;

/// Upstream impl name. `x-` marks a non-standard name upstream never registers.
pub const IMPL_NAME: &str = "x-json";
/// Writers refuse, and readers ignore, JSON larger than this.
pub const MAX_JSON_BYTES: usize = 1024 * 1024;
/// Writers refuse, and readers ignore, schema names longer than this.
pub const MAX_SCHEMA_BYTES: usize = 256;

const IMPL_NAME_C: &CStr = c"x-json";
// The impl name `x-json-N` and this quark's `.vN` suffix change together.
const SLOT_QUARK: &str = "gstsmith.analytics.x-json.v1";
const HEADER_BYTES: usize = 8;

#[repr(transparent)]
struct MtdImpl(ffi::GstAnalyticsMtdImpl);

// SAFETY: the impl is never mutated; it holds a pointer to a 'static C string,
// no callbacks and null reserved fields, all of which may be shared by threads.
unsafe impl Sync for MtdImpl {}

static LOCAL_IMPL: MtdImpl = MtdImpl(ffi::GstAnalyticsMtdImpl {
    name: IMPL_NAME_C.as_ptr(),
    mtd_meta_transform: None,
    mtd_meta_clear: None,
    _reserved: [std::ptr::null_mut(); 19],
});

static RESOLVED: AtomicUsize = AtomicUsize::new(0);

/// Marker type for JSON documents, usable with the `gstreamer-analytics`
/// iteration and relation API (`meta.iter::<JsonMtd>()`, `downcast`, ...).
///
/// The first call to `mtd_type()` reads the default registry, so it panics
/// before `gst::init()`, like the rest of the `GStreamer` API.
#[derive(Debug)]
pub enum JsonMtd {}

// SAFETY: `mtd_type` returns the address of a live `GstAnalyticsMtdImpl` named
// `x-json` whose data uses the layout this module reads and writes.
unsafe impl AnalyticsMtd for JsonMtd {
    fn mtd_type() -> ffi::GstAnalyticsMtdType {
        let cached = RESOLVED.load(Ordering::Acquire);
        if cached != 0 {
            return cached;
        }
        let resolved = resolve_shared_impl();
        RESOLVED.store(resolved, Ordering::Release);
        resolved
    }
}

/// Address of this library's own impl static, before process-wide resolution.
/// Only useful to show that two libraries really carry separate copies.
#[doc(hidden)]
#[must_use]
pub fn local_impl_address() -> usize {
    std::ptr::from_ref(&LOCAL_IMPL) as usize
}

fn resolve_shared_impl() -> ffi::GstAnalyticsMtdType {
    let local = std::ptr::from_ref(&LOCAL_IMPL).cast_mut().cast::<c_void>();
    let registry = gst::Registry::get();
    let object: *mut gobject_ffi::GObject = registry.as_ptr().cast();
    let quark = glib::Quark::from_str(SLOT_QUARK).into_glib();
    // SAFETY: `object` is the live default registry, kept alive by `registry`.
    // With `oldval = NULL` the call installs `local` only if the slot is empty,
    // atomically under GObject's qdata lock. The static needs no destroy notify.
    let installed = unsafe {
        gobject_ffi::g_object_replace_qdata(
            object,
            quark,
            std::ptr::null_mut(),
            local,
            None,
            std::ptr::null_mut(),
        )
    };
    let winner = if installed == glib::ffi::GFALSE {
        // SAFETY: as above; the slot is set and never cleared, so this reads the
        // impl another library installed.
        unsafe { gobject_ffi::g_object_get_qdata(object, quark) }
    } else {
        local
    };
    let winner = winner as ffi::GstAnalyticsMtdType;
    if winner != local as ffi::GstAnalyticsMtdType && !has_expected_name(winner) {
        // A foreign impl under our key would make documents unreadable to
        // anyone else, so keep them local and readable within this library.
        gst::error!(
            crate::CAT,
            "slot {SLOT_QUARK} holds an Mtd impl not named {IMPL_NAME}; JSON documents stay local"
        );
        return local as ffi::GstAnalyticsMtdType;
    }
    winner
}

fn has_expected_name(mtd_type: ffi::GstAnalyticsMtdType) -> bool {
    if mtd_type == 0 {
        return false;
    }
    // SAFETY: this assumes the quark namespace `gstsmith.analytics.x-json.*` is
    // ours alone, so a non-zero slot value is a `GstAnalyticsMtdImpl` address
    // installed by a library that never unloads; get_name reads its name.
    let name = unsafe { ffi::gst_analytics_mtd_type_get_name(mtd_type) };
    // SAFETY: a non-null name is a NUL-terminated C string with static storage.
    !name.is_null() && unsafe { CStr::from_ptr(name) } == IMPL_NAME_C
}

/// A JSON document borrowed from a buffer's relation meta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonDocument<'a> {
    pub id: u32,
    pub schema: &'a str,
    pub json: &'a str,
}

/// Checks a schema name: non-empty `[a-z0-9._/-]`, so it is safe as a JSON
/// key, a log token or a file-name fragment.
fn validate_schema(schema: &str) -> Result<(), glib::BoolError> {
    if schema.is_empty()
        || schema.len() > MAX_SCHEMA_BYTES
        || !schema
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/-".contains(&b))
    {
        return Err(glib::bool_error!("invalid JSON document schema {schema:?}"));
    }
    Ok(())
}

/// Attaches `json`, which must be a valid JSON text, under `schema` and
/// returns the new Mtd id. Use [`add_value`] for data that is not yet text.
pub fn add(
    meta: &mut gst::MetaRefMut<'_, AnalyticsRelationMeta, gst::meta::Standalone>,
    schema: &str,
    json: &str,
) -> Result<u32, glib::BoolError> {
    validate_schema(schema)?;
    check_json_size(schema, json.len())?;
    serde_json::from_str::<serde::de::IgnoredAny>(json)
        .map_err(|err| glib::bool_error!("{schema} document is not JSON: {err}"))?;
    add_checked(meta, schema, json)
}

/// Serializes `value` once and attaches it under `schema`.
pub fn add_value<T: Serialize + ?Sized>(
    meta: &mut gst::MetaRefMut<'_, AnalyticsRelationMeta, gst::meta::Standalone>,
    schema: &str,
    value: &T,
) -> Result<u32, glib::BoolError> {
    validate_schema(schema)?;
    let json = serde_json::to_string(value)
        .map_err(|err| glib::bool_error!("serializing {schema} document: {err}"))?;
    check_json_size(schema, json.len())?;
    add_checked(meta, schema, &json)
}

fn check_json_size(schema: &str, len: usize) -> Result<(), glib::BoolError> {
    if len > MAX_JSON_BYTES {
        return Err(glib::bool_error!(
            "{schema} document of {len} bytes exceeds the {MAX_JSON_BYTES} byte cap"
        ));
    }
    Ok(())
}

fn add_checked(
    meta: &mut gst::MetaRefMut<'_, AnalyticsRelationMeta, gst::meta::Standalone>,
    schema: &str,
    json: &str,
) -> Result<u32, glib::BoolError> {
    let (Ok(schema_len), Ok(json_len)) = (u32::try_from(schema.len()), u32::try_from(json.len()))
    else {
        return Err(glib::bool_error!("JSON document too large"));
    };
    let size = HEADER_BYTES + schema.len() + json.len();
    let mtd_type = JsonMtd::mtd_type();
    // C writes the handle only when it reserved the bytes; on failure it
    // returns a non-null but invalid pointer, so the handle is the signal.
    let mut handle = ffi::GstAnalyticsMtd {
        id: u32::MAX,
        meta: std::ptr::null_mut(),
    };
    // SAFETY: meta is exclusively borrowed and `mtd_type` is a live impl
    // address. `handle` is a valid out-pointer for the duration of the call.
    let data = unsafe {
        ffi::gst_analytics_relation_meta_add_mtd(
            meta.as_mut_ptr(),
            mtd_type as *const ffi::GstAnalyticsMtdImpl,
            size,
            &raw mut handle,
        )
    };
    if handle.meta.is_null() || data.is_null() {
        return Err(glib::bool_error!("could not attach {schema} document"));
    }
    // SAFETY: C filled `handle`, so it reserved `size` bytes at `data`, owned
    // by the exclusively borrowed meta. Every reserved byte is written below,
    // which keeps the reader's `&[u8]` over all `size` bytes initialized.
    let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), size) };
    let (header, payload) = out.split_at_mut(HEADER_BYTES);
    let (schema_out, json_out) = payload.split_at_mut(schema.len());
    let (schema_len_out, json_len_out) = header.split_at_mut(4);
    schema_len_out.copy_from_slice(&schema_len.to_ne_bytes());
    json_len_out.copy_from_slice(&json_len.to_ne_bytes());
    schema_out.copy_from_slice(schema.as_bytes());
    json_out.copy_from_slice(json.as_bytes());
    Ok(handle.id)
}

/// Installs a leaked impl named `name` in the process-wide slot, as a foreign
/// library would, and returns its address. Tests only; it must run before
/// anything in the process resolves `JsonMtd`.
#[doc(hidden)]
#[must_use]
pub fn seed_slot_for_tests(name: &'static CStr) -> Option<usize> {
    let foreign: &'static MtdImpl = Box::leak(Box::new(MtdImpl(ffi::GstAnalyticsMtdImpl {
        name: name.as_ptr(),
        ..LOCAL_IMPL.0
    })));
    let address = std::ptr::from_ref(foreign).cast_mut().cast::<c_void>();
    let registry = gst::Registry::get();
    let quark = glib::Quark::from_str(SLOT_QUARK).into_glib();
    // SAFETY: as in `resolve_shared_impl`; the leaked impl lives forever.
    let installed = unsafe {
        gobject_ffi::g_object_replace_qdata(
            registry.as_ptr().cast(),
            quark,
            std::ptr::null_mut(),
            address,
            None,
            std::ptr::null_mut(),
        )
    };
    (installed != glib::ffi::GFALSE).then_some(address as usize)
}

/// Reads one document's schema and JSON. Malformed or oversized data reads as
/// `None`, never as a panic.
pub trait JsonMtdExt<'a> {
    fn document(&self) -> Option<JsonDocument<'a>>;
}

impl<'a> JsonMtdExt<'a> for AnalyticsMtdRef<'a, JsonMtd> {
    fn document(&self) -> Option<JsonDocument<'a>> {
        // SAFETY: the handle refers to this live, immutably borrowed meta.
        let raw = unsafe { ffi::GstAnalyticsMtd::unsafe_from(self) };
        // SAFETY: as above; size and data are read for an id of this type.
        let size = unsafe { ffi::gst_analytics_mtd_get_size(&raw const raw) };
        // SAFETY: as above.
        let data = unsafe { ffi::gst_analytics_relation_meta_get_mtd_data(raw.meta, raw.id) };
        if data.is_null() || size < HEADER_BYTES {
            return None;
        }
        // SAFETY: the meta owns `size` initialized bytes at `data`. They stay
        // valid and unchanged for 'a, because adding an Mtd needs the buffer
        // writable, which the 'a borrow of the buffer excludes.
        let bytes: &'a [u8] = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) };
        parse(self.id(), bytes)
    }
}

fn parse(id: u32, bytes: &[u8]) -> Option<JsonDocument<'_>> {
    let (header, rest) = bytes.split_first_chunk::<HEADER_BYTES>()?;
    let (schema_len, json_len) = header.split_first_chunk::<4>()?;
    let schema_len = usize::try_from(u32::from_ne_bytes(*schema_len)).ok()?;
    let json_len = usize::try_from(u32::from_ne_bytes(json_len.try_into().ok()?)).ok()?;
    if schema_len > MAX_SCHEMA_BYTES || json_len > MAX_JSON_BYTES {
        return None;
    }
    let schema = std::str::from_utf8(rest.get(..schema_len)?).ok()?;
    let json =
        std::str::from_utf8(rest.get(schema_len..schema_len.checked_add(json_len)?)?).ok()?;
    Some(JsonDocument { id, schema, json })
}

/// Reads the document with Mtd id `id`, if it is a `JsonMtd`.
#[must_use]
pub fn get<'a>(
    meta: &gst::MetaRef<'a, AnalyticsRelationMeta>,
    id: u32,
) -> Option<JsonDocument<'a>> {
    use gst_analytics::AnalyticsMetaRefExt as _;
    meta.mtd::<JsonMtd>(id)?.document()
}

/// All readable documents in the meta, in Mtd id order.
///
/// Like upstream's `iter`, this borrows the `MetaRef` for its own lifetime:
/// bind `buffer.meta::<AnalyticsRelationMeta>()` to a local first.
pub fn documents<'a>(
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
) -> impl Iterator<Item = JsonDocument<'a>> + 'a {
    use gst_analytics::AnalyticsMetaRefExt as _;
    meta.iter::<JsonMtd>().filter_map(|mtd| mtd.document())
}

/// Documents with the given schema.
pub fn documents_with_schema<'a>(
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
    schema: &'a str,
) -> impl Iterator<Item = JsonDocument<'a>> + 'a {
    documents(meta).filter(move |doc| doc.schema == schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(result: Result<u32, glib::BoolError>) -> bool {
        result.err().is_some()
    }

    fn raw(schema: &[u8], json: &[u8], schema_len: u32, json_len: u32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&schema_len.to_ne_bytes());
        data.extend_from_slice(&json_len.to_ne_bytes());
        data.extend_from_slice(schema);
        data.extend_from_slice(json);
        data
    }

    #[test]
    fn parse_rejects_truncated_and_oversized_data() {
        assert_eq!(parse(1, &[]), None);
        assert_eq!(parse(1, &[1, 0, 0, 0, 2, 0, 0]), None);
        let data = raw(b"s", b"{}", 1, 2);
        assert_eq!(
            parse(7, &data),
            Some(JsonDocument {
                id: 7,
                schema: "s",
                json: "{}"
            })
        );
        assert_eq!(parse(7, &data[..data.len() - 1]), None);
        assert_eq!(parse(1, &raw(b"s", b"", 1, u32::MAX)), None);
        assert_eq!(parse(1, &raw(b"\xff", b"{}", 1, 2)), None);
    }

    #[test]
    fn add_validates_schema_size_and_json() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        let mut meta = AnalyticsRelationMeta::add(buffer.make_mut());
        for schema in ["", "X-Caption", "x caption", "x=caption", "x\"y"] {
            assert!(
                rejected(add(&mut meta, schema, "{}")),
                "{schema:?} accepted"
            );
        }
        assert!(rejected(add(&mut meta, "x-caption", "{")));
        let big = format!("\"{}\"", "a".repeat(MAX_JSON_BYTES));
        assert!(rejected(add(&mut meta, "x-caption", &big)));
        assert!(rejected(add_value(&mut meta, "x-caption", &big)));
        let id = add(&mut meta, "example/detector/record", r#"{"text":"héllo"}"#)
            .expect("valid document attaches");
        let value_id = add_value(&mut meta, "x-caption", &serde_json::json!({"text": "x"}))
            .expect("serialized document attaches");
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        assert_eq!(
            get(&meta, id),
            Some(JsonDocument {
                id,
                schema: "example/detector/record",
                json: r#"{"text":"héllo"}"#
            })
        );
        assert_eq!(
            get(&meta, value_id).map(|doc| doc.json),
            Some(r#"{"text":"x"}"#)
        );
        assert_eq!(get(&meta, value_id + 1), None);
        assert_eq!(documents_with_schema(&meta, "x-caption").count(), 1);
    }

    #[test]
    fn resolves_to_the_local_impl_without_competitors() {
        gst::init().expect("GStreamer initializes");
        assert_eq!(JsonMtd::mtd_type(), local_impl_address());
        assert!(has_expected_name(JsonMtd::mtd_type()));
    }
}
