// standard
use std::collections::HashSet;
use std::ffi::CString;
use std::ptr;
use std::io::{Cursor, Read};
use std::os::raw::{c_void, c_char, c_int};
#[cfg(unix)]
use std::os::unix::io::{IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{IntoRawSocket, RawSocket};
use std::panic;
use std::path::Path;
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

// extern crates
use anyhow::{Result, bail, ensure};
use bson::doc;

// internal crates
use crate::object_registry::*;
use crate::define_registry;
use crate::tor_crypto::*;
use crate::tor_controller::*;
use crate::gosling::*;

// todo: functions should catch all errors and return nice error messages, no '?' or unwrap()'s here
// todo: implement a customizable logger for internal debug logging and purge printlns throughout the library
/// Error Handling

pub struct Error {
    message: CString,
}

impl Error {
    pub fn new(message: &str) -> Error {
        Error{message: CString::new(message).unwrap()}
    }
}

define_registry!{Error, ObjectTypes::Error}

// exported C type
pub struct GoslingError;

#[no_mangle]
/// Get error message from gosling_error
///
/// @param error : the error object to get the message from
/// @return : null terminated string with error message whose
///  lifetime is tied to the source
pub extern "C" fn gosling_error_get_message(error: *const GoslingError) -> *const c_char {
    if !error.is_null() {
        let key = error as usize;

        let registry = get_error_registry();
        if registry.contains_key(key) {
            if let Some(x) = registry.get(key) {
                return x.message.as_ptr();
            }
        }
    }

    ptr::null()
}

// macro for defining the implmenetation of freeing objects
// owned by an ObjectRegistry
macro_rules! impl_registry_free {
    ($obj:expr, $type:ty) => {
        if $obj.is_null() {
            return;
        }

        let key = $obj as usize;
        paste::paste! {
            [<get_ $type:snake _registry>]().remove(key);
        }
    }
}

/// Frees gosling_error and invalidates any message strings
/// returned by gosling_error_get_message() from the given
/// error object.
///
/// @param error : the error object to free
#[no_mangle]
pub extern "C" fn gosling_error_free(error: *mut GoslingError) {
    impl_registry_free!(error, Error);
}

pub struct GoslingEd25519PrivateKey;
pub struct GoslingX25519PrivateKey;
pub struct GoslingX25519PublicKey;
pub struct GoslingV3OnionServiceId;
pub struct GoslingContext;
pub struct GoslingIdentityClientHandshake;
pub struct GoslingIdentityServerHandshake;

define_registry!{Ed25519PrivateKey, ObjectTypes::Ed25519PrivateKey}
define_registry!{X25519PrivateKey, ObjectTypes::X25519PrivateKey}
define_registry!{X25519PublicKey, ObjectTypes::X25519PublicKey}
define_registry!{V3OnionServiceId, ObjectTypes::V3OnionServiceId}

/// cbindgen:ignore
type ContextTuple = (Context<NativeIdentityClientHandshake, NativeIdentityServerHandshake>, EventCallbacks);

define_registry!{ContextTuple, ObjectTypes::Context}

/// Frees a gosling_ed25519_private_key object
///
/// @param private_key : the private key to free
#[no_mangle]
pub extern "C" fn gosling_ed25519_private_key_free(private_key: *mut GoslingEd25519PrivateKey) {
    impl_registry_free!(private_key, Ed25519PrivateKey);
}

/// Frees a gosling_x25519_private_key object
///
/// @param private_key : the private key to free
#[no_mangle]
pub extern "C" fn gosling_x25519_private_key_free(private_key: *mut GoslingX25519PrivateKey) {
    impl_registry_free!(private_key, X25519PrivateKey);
}
/// Frees a gosling_x25519_public_key object
///
/// @param public_key : the public key to free
#[no_mangle]
pub extern "C" fn gosling_x25519_public_key_free(public_key: *mut GoslingX25519PublicKey) {
    impl_registry_free!(public_key, X25519PublicKey);
}
/// Frees a gosling_v3_onion_service_id object
///
/// @param service_id : the service id object to free
#[no_mangle]
pub extern "C" fn gosling_v3_onion_service_id_free(service_id: *mut GoslingV3OnionServiceId) {
    impl_registry_free!(service_id, V3OnionServiceId);
}
/// Frees a gosling_context object
///
/// @param context : the context object to free
#[no_mangle]
pub extern "C" fn gosling_context_free(context: *mut GoslingContext) {
    impl_registry_free!(context, ContextTuple);
}
/// Frees a gosling_identity_client_handshake object
///
/// @param handshake : the handshake object to free
#[no_mangle]
pub extern "C" fn gosling_identity_client_handshake_free(handshake: *mut GoslingIdentityClientHandshake) {
    impl_registry_free!(handshake, NativeIdentityClientHandshake);
}
/// Frees a gosling_identity_server_handshake object
///
/// @param handshake : the handshake object to free
#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_free(handshake: *mut GoslingIdentityServerHandshake) {
    impl_registry_free!(handshake, NativeIdentityServerHandshake);
}

/// Wrapper around rust code which may panic or return a failing Result to be used at FFI boundaries.
/// Converts panics or error Results into GoslingErrors if a memory location is provided.
///
/// @param default : The default value to return in the event of failure
/// @param out_error : A pointer to pointer to GoslingError 'struct' for the C FFI
/// @param closure : The functionality we need to encapsulate behind the error handling logic
/// @return : The result of closure() on success, or the value of default on failure.
fn translate_failures<R,F>(default: R, out_error: *mut *mut GoslingError, closure:F) -> R where F: FnOnce() -> Result<R> + panic::UnwindSafe {
    match panic::catch_unwind(closure) {
        // handle success
        Ok(Ok(retval)) => {
            retval
        },
        // handle runtime error
        Ok(Err(err)) => {
            if !out_error.is_null() {
                // populate error with runtime error message
                let key = get_error_registry().insert(Error::new(format!("{:?}", err).as_str()));
                unsafe {*out_error = key as *mut GoslingError;};
            }
            default
        },
        // handle panic
        Err(_) => {
            if !out_error.is_null() {
                // populate error with panic message
                let key = get_error_registry().insert(Error::new("panic occurred"));
                unsafe {*out_error = key as *mut GoslingError;};
            }
            default
        },
    }
}

/// Creation method for securely generating a new gosling_ed25510_private_key
///
/// @param out_privateKey : returned generated ed25519 private key
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_ed25519_private_key_generate(
    out_private_key: *mut *mut GoslingEd25519PrivateKey,
    error: *mut *mut GoslingError) {
    translate_failures((), error, || -> Result<()> {
        if out_private_key.is_null() {
            bail!("gosling_ed25519_private_key_generate(): out_private_key must not be null");
        }

        let private_key = Ed25519PrivateKey::generate();
        let handle = get_ed25519_private_key_registry().insert(private_key);
        unsafe { *out_private_key = handle as *mut GoslingEd25519PrivateKey };

        Ok(())
    })
}

/// Conversion method for converting the KeyBlob string returned by ADD_ONION
/// command into a gosling_ed25519_private_key
///
/// @param out_private_key : returned ed25519 private key
/// @param key_blob : an ed25519 KeyBlob string in the form
///  "ED25519-V3:abcd1234..."
/// @param key_blob_length : number of characters in keyBlob not counting the
///  null terminator
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_ed25519_private_key_from_keyblob(
    out_private_key: *mut *mut GoslingEd25519PrivateKey,
    key_blob: *const c_char,
    key_blob_length: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if out_private_key.is_null() {
            bail!("gosling_ed25519_private_key_from_keyblob(): out_private_key must not be null");
        }

        if key_blob.is_null() {
            bail!("gosling_ed25519_private_key_from_keyblob(): key_blob must not not be null");
        }

        if key_blob_length != ED25519_KEYBLOB_LENGTH {
            bail!("gosling_ed25519_private_key_from_keyblob(): key_blob_length must be exactly ED25519_KEYBLOB_LENGTH ({}); received '{}'", ED25519_KEYBLOB_LENGTH, key_blob_length);
        }

        let key_blob_view = unsafe { std::slice::from_raw_parts(key_blob as *const u8, key_blob_length) };
        let key_blob_str = std::str::from_utf8(key_blob_view)?;
        let private_key = Ed25519PrivateKey::from_key_blob(key_blob_str)?;

        let handle = get_ed25519_private_key_registry().insert(private_key);
        unsafe { *out_private_key = handle as *mut GoslingEd25519PrivateKey };

        Ok(())
    })
}

/// Conversion method for converting an ed25519 private key to a null-
///  terminated KeyBlob string for use with ADD_ONION command
///
/// @param private_key : the private key to encode
/// @param out_key_blob : buffer to be filled with ed25519 KeyBlob in
///  the form "ED25519-V3:abcd1234...\0"
/// @param key_blob_size : size of out_key_blob buffer in bytes, must be at
///  least 100 characters (99 for string + 1 for null terminator)
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_ed25519_private_key_to_keyblob(
    private_key: *const GoslingEd25519PrivateKey,
    out_key_blob: *mut c_char,
    key_blob_size: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if private_key.is_null() {
            bail!("gosling_ed25519_private_key_to_keyblob(): private_key must not be null");
        }

        if out_key_blob.is_null() {
            bail!("gosling_ed25519_private_key_to_keyblob(): out_key_blob must not be null");
        }

        if key_blob_size < ED25519_KEYBLOB_SIZE {
            bail!("gosling_ed25519_private_key_to_keyblob(): key_blob_size must be at least '{}', received '{}'", ED25519_KEYBLOB_SIZE, key_blob_size);
        }

        let registry = get_ed25519_private_key_registry();
        match registry.get(private_key as usize) {
            Some(private_key) => {
                let private_key_blob = private_key.to_key_blob();
                unsafe {
                    // copy keyblob into output buffer
                    let key_blob_view = std::slice::from_raw_parts_mut(out_key_blob as *mut u8, key_blob_size);
                    std::ptr::copy(private_key_blob.as_ptr(), key_blob_view.as_mut_ptr(), ED25519_KEYBLOB_LENGTH);
                    // add final null-terminator
                    key_blob_view[ED25519_KEYBLOB_LENGTH] = 0u8;
                };
            },
            None => {
                bail!("gosling_ed25519_private_key_to_keyblob(): private_key is invalid");
            },
        };

        Ok(())
    })
}

/// Conversion method for converting a base64-encoded string used by the
/// ONION_CLIENT_AUTH_ADD command into a gosling_x25519_private_key
///
/// @param out_private_key : returned x25519 private key
/// @param base64 : an x25519 private key encoded as a base64 string
/// @param base64_length : number of characters in base64 not counting any
///  terminator
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_x25519_private_key_from_base64(
    out_private_key: *mut *mut GoslingX25519PrivateKey,
    base64: *const c_char,
    base64_length: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if out_private_key.is_null() {
            bail!("gosling_x25519_private_key_from_base64(): out_private_key must not be null");
        }

        if base64.is_null() {
            bail!("gosling_x25519_private_key_from_base64(): base64 must not not be null");
        }

        if base64_length != X25519_PRIVATE_KEYBLOB_BASE64_LENGTH {
            bail!("gosling_x25519_private_key_from_base64(): base64_length must be exactly X25519_PRIVATE_KEYBLOB_BASE64_LENGTH ({}); received '{}'", X25519_PRIVATE_KEYBLOB_BASE64_LENGTH, base64_length);
        }

        let base64_view = unsafe { std::slice::from_raw_parts(base64 as *const u8, base64_length) };
        let base64_str = std::str::from_utf8(base64_view)?;
        let private_key = X25519PrivateKey::from_base64(base64_str)?;

        let handle = get_x25519_private_key_registry().insert(private_key);
        unsafe { *out_private_key = handle as *mut GoslingX25519PrivateKey };

        Ok(())
    })
}

/// Conversion method for converting an x25519 private key to a null-
///  terminated base64 string for use with ONION_CLIENT_AUTH_ADD command
///
/// @param private_key : the private key to encode
/// @param out_base64 : buffer to be filled with x25519 key encoded as base64
/// @param base64_size : size of out_base64 buffer in bytes, must be at
///  least 45 characters (44 for string + 1 for null terminator)
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_x25519_private_key_to_base64(
    private_key: *const GoslingX25519PrivateKey,
    out_base64: *mut c_char,
    base64_size: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if private_key.is_null() {
            bail!("gosling_x25519_private_key_to_base64(): private_key must not be null");
        }

        if out_base64.is_null() {
            bail!("gosling_x25519_private_key_to_base64(): out_base64 must not be null");
        }

        if base64_size < X25519_PRIVATE_KEYBLOB_BASE64_SIZE {
            bail!("gosling_x25519_private_key_to_base64(): base64_size must be at least '{}', received '{}'", X25519_PRIVATE_KEYBLOB_BASE64_SIZE, base64_size);
        }

        let registry = get_x25519_private_key_registry();
        match registry.get(private_key as usize) {
            Some(private_key) => {
                let private_key_blob = private_key.to_base64();
                unsafe {
                    // copy base64 into output buffer
                    let base64_view = std::slice::from_raw_parts_mut(out_base64 as *mut u8, base64_size);
                    std::ptr::copy(private_key_blob.as_ptr(), base64_view.as_mut_ptr(), X25519_PRIVATE_KEYBLOB_BASE64_LENGTH);
                    // add final null-terminator
                    base64_view[X25519_PRIVATE_KEYBLOB_BASE64_LENGTH] = 0u8;
                };
            },
            None => {
                bail!("gosling_x25519_private_key_to_base64(): private_key is invalid");
            },
        };

        Ok(())
    })
}

/// Conversion method for converting a base32-encoded string used by the
/// ADD_ONION command into a gosling_x25519_public_key
///
/// @param out_public_key : returned x25519 public key
/// @param base32 : an x25519 public key encoded as a base32 string
/// @param base32_length : number of characters in base32 not counting any
///  terminator
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_x25519_public_key_from_base32(
    out_public_key: *mut *mut GoslingX25519PublicKey,
    base32: *const c_char,
    base32_length: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if out_public_key.is_null() {
            bail!("gosling_x25519_public_key_from_base32(): out_public_key must not be null");
        }

        if base32.is_null() {
            bail!("gosling_x25519_public_key_from_base32(): base32 must not not be null");
        }

        if base32_length != X25519_PUBLIC_KEYBLOB_BASE32_LENGTH {
            bail!("gosling_x25519_public_key_from_base32(): base32_length must be exactly X25519_PUBLIC_KEYBLOB_BASE32_LENGTH ({}); received '{}'", X25519_PUBLIC_KEYBLOB_BASE32_LENGTH, base32_length);
        }

        let base32_view = unsafe { std::slice::from_raw_parts(base32 as *const u8, base32_length) };
        let base32_str = std::str::from_utf8(base32_view)?;
        let public_key = X25519PublicKey::from_base32(base32_str)?;

        let handle = get_x25519_public_key_registry().insert(public_key);
        unsafe { *out_public_key = handle as *mut GoslingX25519PublicKey };

        Ok(())
    })
}

/// Conversion method for converting an x25519 public key to a null-
/// terminated base64 string for use with ADD_ONION command
///
/// @param public_key : the public key to encode
/// @param out_base32 : buffer to be filled with x25519 key encoded as base32
/// @param base32_size : size of out_base32 buffer in bytes, must be at
///  least 53 characters (52 for string + 1 for null terminator)
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_x25519_public_key_to_base32(
    public_key: *const GoslingX25519PublicKey,
    out_base32: *mut c_char,
    base32_size: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if public_key.is_null() {
            bail!("gosling_x25519_public_key_to_base32(): public must not be null");
        }

        if out_base32.is_null() {
            bail!("gosling_x25519_public_key_to_base32(): out_base32 must not be null");
        }

        if base32_size < X25519_PUBLIC_KEYBLOB_BASE32_SIZE {
            bail!("gosling_x25519_public_key_to_base32(): base32_size must be at least '{}', received '{}'", X25519_PUBLIC_KEYBLOB_BASE32_SIZE, base32_size);
        }

        let registry = get_x25519_public_key_registry();
        match registry.get(public_key as usize) {
            Some(public_key) => {
                let public_base32 = public_key.to_base32();
                unsafe {
                    // copy base32 into output buffer
                    let base32_view = std::slice::from_raw_parts_mut(out_base32 as *mut u8, base32_size);
                    std::ptr::copy(public_base32.as_ptr(), base32_view.as_mut_ptr(), X25519_PUBLIC_KEYBLOB_BASE32_LENGTH);
                    // add final null-terminator
                    base32_view[X25519_PUBLIC_KEYBLOB_BASE32_LENGTH] = 0u8;
                };
            },
            None => {
                bail!("gosling_x25519_public_key_to_base32(): public_key is invalid");
            },
        };

        Ok(())
    })
}

/// Conversion method for converting a v3 onion service string into a
/// gosling_v3_onion_service_id object
///
/// @param out_service_id : returned service id object
/// @param service_id_string : a v3 onion service id string
/// @param service_id_string_length : number of characters in service_id_string
///  not counting any null terminator
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_v3_onion_service_id_from_string(
    out_service_id: *mut *mut GoslingV3OnionServiceId,
    service_id_string: *const c_char,
    service_id_string_length: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if out_service_id.is_null() {
            bail!("gosling_v3_onion_service_id_from_string(): out_service_id must not be null");
        }

        if service_id_string.is_null() {
            bail!("gosling_v3_onion_service_id_from_string(): service_id_string must not not be null");
        }

        if service_id_string_length != V3_ONION_SERVICE_ID_LENGTH {
            bail!("gosling_v3_onion_service_id_from_string(): base32_length must be exactly V3_ONION_SERVICE_ID_LENGTH ({}); received '{}'", V3_ONION_SERVICE_ID_LENGTH, service_id_string_length);
        }

        let service_id_view = unsafe { std::slice::from_raw_parts(service_id_string as *const u8, service_id_string_length) };
        let service_id_str = std::str::from_utf8(service_id_view)?;
        let service_id = V3OnionServiceId::from_string(service_id_str)?;

        let handle = get_v3_onion_service_id_registry().insert(service_id);
        unsafe { *out_service_id = handle as *mut GoslingV3OnionServiceId };

        Ok(())
    })
}

/// Conversion method for converting v3 onion service id to a null-terminated
/// string
///
/// @param service_id : the service id to encode
/// @param out_service_id_string : buffer to be filled with x25519 key encoded as base32
/// @param service_id_string_size : size of out_service_id_string buffer in bytes,
///  must be at least 57 characters (56 for string + 1 for null terminator)
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_v3_onion_service_id_to_string(
    service_id: *const GoslingV3OnionServiceId,
    out_service_id_string: *mut c_char,
    service_id_string_size: usize,
    error: *mut *mut GoslingError) {

    translate_failures((), error, || -> Result<()> {
        if service_id.is_null() {
            bail!("gosling_v3_onion_service_id_to_string(): service_id must not be null");
        }

        if out_service_id_string.is_null() {
            bail!("gosling_v3_onion_service_id_to_string(): out_service_id_string must not be null");
        }

        if service_id_string_size < V3_ONION_SERVICE_ID_SIZE {
            bail!("gosling_v3_onion_service_id_to_string(): service_id_string_size must be at least '{}', received '{}'", V3_ONION_SERVICE_ID_SIZE, service_id_string_size);
        }

        let registry = get_v3_onion_service_id_registry();
        match registry.get(service_id as usize) {
            Some(service_id) => {
                let service_id_string = service_id.to_string();
                unsafe {
                    // copy service_id_string into output buffer
                    let service_id_string_view = std::slice::from_raw_parts_mut(out_service_id_string as *mut u8, service_id_string_size);
                    std::ptr::copy(service_id_string.as_ptr(), service_id_string_view.as_mut_ptr(), V3_ONION_SERVICE_ID_LENGTH);
                    // add final null-terminator
                    service_id_string_view[V3_ONION_SERVICE_ID_LENGTH] = 0u8;
                };
            },
            None => {
                bail!("gosling_v3_onion_service_id_to_string(): service_id is invalid");
            },
        };

        Ok(())
    })
}

/// Checks if a service id string is valid per tor rend spec:
/// https://gitweb.torproject.org/torspec.git/tree/rend-spec-v3.txt
///
/// @param service_id_string : string containing the v3 service id to be validated
/// @param service_id_string_length : length of serviceIdString not counting the
///  null terminator; must be V3_ONION_SERVICE_ID_LENGTH (56)
/// @param error : filled on error
#[no_mangle]
pub extern "C" fn gosling_string_is_valid_v3_onion_service_id(
    service_id_string: *const c_char,
    service_id_string_length: usize,
    error: *mut *mut GoslingError) -> bool {

    translate_failures(false, error, || -> Result<bool> {
        if service_id_string.is_null() {
            bail!("gosling_string_is_valid_v3_onion_service_id(): service_id_string must not be null");
        }

        if service_id_string_length != V3_ONION_SERVICE_ID_LENGTH {
            bail!("gosling_string_is_valid_v3_onion_service_id(): service_id_string_length must be V3_ONION_SERVICE_ID_LENGTH (56); received '{}'", service_id_string_length);
        }

        let service_id_string_slice = unsafe { std::slice::from_raw_parts(service_id_string as *const u8, service_id_string_length) };
        Ok(V3OnionServiceId::is_valid(str::from_utf8(service_id_string_slice)?))
    })
}

// shared between client and server handshakes to avoid accidental collisions
lazy_static! {
    static ref NEXT_HANDSHAKE_HANDLE: AtomicUsize = Default::default();
}

///
/// Client Handshake
///

pub type GoslingIdentityClientHandshakeStartedCallback = extern fn(
    handshake_handle: usize) -> ();

pub type GoslingIdentityClientHandshakeChallengeResponseSizeCallback = extern fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize) -> usize;

pub type GoslingIdentityClientHandshakeBuildChallengeResponseCallback = extern fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    challenge_buffer: *const u8,
    challenge_buffer_size: usize,
    out_challenge_response_buffer: *mut u8,
    challenge_response_buffer_size: usize) -> ();

#[derive(Default)]
pub struct NativeIdentityClientHandshake {
    handshake_handle: usize,
    started_callback: Option<GoslingIdentityClientHandshakeStartedCallback>,
    challenge_response_size_callback: Option<GoslingIdentityClientHandshakeChallengeResponseSizeCallback>,
    build_challenge_response_callback: Option<GoslingIdentityClientHandshakeBuildChallengeResponseCallback>,
}

impl Clone for NativeIdentityClientHandshake {
    fn clone(&self) -> Self {
        // needs to dupicate the callbacks of the prototype handshake, and invoke the
        // started_callback with a new unique handle
        let handshake_handle = NEXT_HANDSHAKE_HANDLE.fetch_add(1, Ordering::Relaxed);
        match self.started_callback {
            Some(started_callback) => started_callback(handshake_handle),
            None => panic!("NativeIdentityClientHandshake::clone(): missing started_callback"),
        }

        Self{
            handshake_handle,
            started_callback: self.started_callback.clone(),
            challenge_response_size_callback: self.challenge_response_size_callback.clone(),
            build_challenge_response_callback: self.build_challenge_response_callback.clone(),
        }
    }
}

impl IdentityClientHandshake for NativeIdentityClientHandshake {
    fn build_challenge_response(&self, endpoint: &str, challenge: &bson::document::Document) -> bson::document::Document {

        let endpoint0 = CString::new(endpoint).unwrap();
        let response_size = match self.challenge_response_size_callback {
            Some(challenge_response_size_callback) => {
                challenge_response_size_callback(self.handshake_handle, endpoint0.as_ptr(), endpoint.len())
            },
            None => panic!("NativeIdentityClientHandshake::build_challenge_response(): missing challenge_response_size_callback"),
        };

        // buffer for response to be written
        let mut response_buffer = vec![0u8; response_size];

        // challenge to bytes for native callback
        let mut challenge_buffer: Vec<u8> = Default::default();
        challenge.to_writer(&mut challenge_buffer).unwrap();

        // build response via the response callback
        match self.build_challenge_response_callback {
            Some(build_challenge_response_callback) => {
                build_challenge_response_callback(
                    self.handshake_handle,
                    endpoint0.as_ptr(),
                    endpoint.len(),
                    challenge_buffer.as_ptr(),
                    challenge_buffer.len(),
                    response_buffer.as_mut_ptr(),
                    response_buffer.len())
            },
            None => panic!("NativeIdentityClientHandshake::build_challenge_response(): missing build_challenge_response_callback"),
        }

        // convert byte buffer to a bson document and return
        match bson::document::Document::from_reader(Cursor::new(response_buffer)) {
            Ok(response) => response,
            Err(_) => panic!("NativeIdentityClientHandshake::build_challenge_response(): build_challenge_response_callback returned invalid bson document"),
        }
    }
}

define_registry!{NativeIdentityClientHandshake, ObjectTypes::IdentityClientHandshake}

#[no_mangle]
pub extern "C" fn gosling_identity_client_handshake_init(
    out_client_handshake: *mut *mut GoslingIdentityClientHandshake,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!out_client_handshake.is_null(), "gosling_identity_client_handshake_init(): out_client_handshake must not be null");
        let handle = get_native_identity_client_handshake_registry().insert(Default::default());
        unsafe {*out_client_handshake = handle as *mut GoslingIdentityClientHandshake };

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_client_handshake_set_started_callback(
    client_handshake: *mut GoslingIdentityClientHandshake,
    callback: GoslingIdentityClientHandshakeStartedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!client_handshake.is_null(), "gosling_identity_client_handshake_set_started_callback(): client_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_client_handshake_set_started_callback(): callback must not be null");

        let mut native_identity_client_registry = get_native_identity_client_handshake_registry();
        let mut client_handshake = match native_identity_client_registry.get_mut(client_handshake as usize) {
            Some(client_handshake) => client_handshake,
            None => bail!("gosling_identity_client_handshake_set_started_callback(): client_handshake is invalid"),
        };
        client_handshake.started_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_client_handshake_set_challenge_response_size_callback(
    client_handshake: *mut GoslingIdentityClientHandshake,
    callback: GoslingIdentityClientHandshakeChallengeResponseSizeCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!client_handshake.is_null(), "gosling_identity_client_handshake_set_challenge_response_size_callback(): client_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_client_handshake_set_challenge_response_size_callback(): callback must not be null");

        let mut native_identity_client_registry = get_native_identity_client_handshake_registry();
        let mut client_handshake = match native_identity_client_registry.get_mut(client_handshake as usize) {
            Some(client_handshake) => client_handshake,
            None => bail!("gosling_identity_client_handshake_set_challenge_response_size_callback(): client_handshake is invalid"),
        };
        client_handshake.challenge_response_size_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_client_handshake_set_build_challenge_response_callback(
    client_handshake: *mut GoslingIdentityClientHandshake,
    callback: GoslingIdentityClientHandshakeBuildChallengeResponseCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!client_handshake.is_null(), "gosling_identity_client_handshake_set_build_challenge_response_callback(): client_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_client_handshake_set_build_challenge_response_callback(): callback must not be null");

        let mut native_identity_client_registry = get_native_identity_client_handshake_registry();
        let mut client_handshake = match native_identity_client_registry.get_mut(client_handshake as usize) {
            Some(client_handshake) => client_handshake,
            None => bail!("gosling_identity_client_handshake_set_build_challenge_response_callback(): client_handshake is invalid"),
        };
        client_handshake.build_challenge_response_callback = Some(callback);
        Ok(())
    });
}

///
/// Server Handshake
///
pub type GoslingIdentityServerHandshakeStartedCallback = extern "C" fn(
    handshake_handle: usize) -> ();

pub type GoslingIdentityServerHandshakeEndpointSupportedCallback = extern "C" fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize) -> bool;

pub type GoslingIdentityServerHandshakeChallengeSizeCallback = extern "C" fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize) -> usize;

pub type GoslingIdentityServerHandshakeBuildChallengeCallback = extern "C" fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    out_challenge_buffer: *mut u8,
    challenge_buffer_size: usize) -> ();

#[repr(C)]
pub enum GoslingChallengeResponseResult {
    Valid,
    Invalid,
    Pending,
}

pub type GoslingIdentityServerHandshakeVerifyChallengeResponseCallback = extern fn(
    handshake_handle: usize,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    challenge_buffer: *const u8,
    challenge_buffer_size: usize,
    challenge_response_buffer: *const u8,
    challenge_response_buffer_size: usize) -> GoslingChallengeResponseResult;

pub type GoslingIdentityServerHandshakePollChallengeResponseResultCallback = extern fn(
    handshake_handle: usize) -> GoslingChallengeResponseResult;

#[derive(Default)]
pub struct NativeIdentityServerHandshake {
    handshake_handle: usize,
    started_callback: Option<GoslingIdentityServerHandshakeStartedCallback>,
    endpoint_supported_callback: Option<GoslingIdentityServerHandshakeEndpointSupportedCallback>,
    challenge_size_callack: Option<GoslingIdentityServerHandshakeChallengeSizeCallback>,
    build_challenge_callback: Option<GoslingIdentityServerHandshakeBuildChallengeCallback>,
    verify_challenge_response_callback: Option<GoslingIdentityServerHandshakeVerifyChallengeResponseCallback>,
    poll_challenge_response_result_callback: Option<GoslingIdentityServerHandshakePollChallengeResponseResultCallback>,
}

impl Clone for NativeIdentityServerHandshake {
    fn clone(&self) -> Self {
        // needs to duplicate the callbacks of the prototype handshake, and invoke the
        // started_callback with a new unique handle
        let handshake_handle = NEXT_HANDSHAKE_HANDLE.fetch_add(1, Ordering::Relaxed);
        match self.started_callback {
            Some(started_callback) => started_callback(handshake_handle),
            None => panic!("NativeIdentityServerHandshake::clone(): missing started_callback"),
        }

        Self{
            handshake_handle,
            started_callback: self.started_callback.clone(),
            endpoint_supported_callback: self.endpoint_supported_callback.clone(),
            challenge_size_callack: self.challenge_size_callack.clone(),
            build_challenge_callback: self.build_challenge_callback.clone(),
            verify_challenge_response_callback: self.verify_challenge_response_callback.clone(),
            poll_challenge_response_result_callback: self.poll_challenge_response_result_callback.clone(),
        }
    }
}

impl IdentityServerHandshake for NativeIdentityServerHandshake {
    fn endpoint_supported(&mut self, endpoint: &str) -> bool {
        // endpoint to cstring
        let endpoint0 = CString::new(endpoint).unwrap();

        match self.endpoint_supported_callback {
            Some(endpoint_supported_callback) => endpoint_supported_callback(
                self.handshake_handle,
                endpoint0.as_ptr(),
                endpoint.len()),
            None => panic!("NativeIdentityServerHandshake::endpoint_supported(): missing endpoint_supported_callback"),
        }
    }

    fn build_endpoint_challenge(&mut self, endpoint: &str) -> Option<bson::document::Document> {
        // endpoint to cstring
        let endpoint0 = CString::new(endpoint).unwrap();

        let challenge_size = match self.challenge_size_callack {
            Some(challenge_size_callack) => challenge_size_callack(
                self.handshake_handle,
                endpoint0.as_ptr(),
                endpoint.len()),
            None => panic!("NativeIdentityServerHandshake::build_endpoint_challenge(): missing challenge_size_callack"),
        };

        // buffer for challenge to be written
        let mut challenge_buffer = vec![0u8; challenge_size];

        // write challenge to buffer
        match self.build_challenge_callback {
            Some(build_challenge_callback) => build_challenge_callback(
                self.handshake_handle,
                endpoint0.as_ptr(),
                endpoint.len(),
                challenge_buffer.as_mut_ptr(),
                challenge_size),
            None => panic!("NativeIdentityServerHandshake::build_endpoint_challenge(): missing build_challenge_callback"),
        }

        // convert byte buffer to a bson document and return
        match bson::document::Document::from_reader(Cursor::new(challenge_buffer)) {
            Ok(challenge) => Some(challenge),
            Err(_) => panic!("NativeIdentityServerHandshake::build_challenge_response(): build_challenge_callback returned invalid bson document"),
        }
    }

    fn verify_challenge_response(&mut self,
                                 endpoint: &str,
                                 challenge: bson::document::Document,
                                 challenge_response: bson::document::Document) -> Option<bool> {
        // epdoint to cstring
        let endpoint0 = CString::new(endpoint).unwrap();

        // get challenge raw bytes
        let mut challenge_buffer: Vec<u8> = Default::default();
        challenge.to_writer(&mut challenge_buffer).unwrap();

        // get response raw bytes
        let mut challenge_response_buffer: Vec<u8> = Default::default();
        challenge_response.to_writer(&mut challenge_response_buffer).unwrap();

        // get challenge response verification result
        let challenge_response_result = match self.verify_challenge_response_callback {
            Some(verify_challenge_response_callback) => verify_challenge_response_callback(
                self.handshake_handle,
                endpoint0.as_ptr(),
                endpoint.len(),
                challenge_buffer.as_ptr(),
                challenge_buffer.len(),
                challenge_response_buffer.as_ptr(),
                challenge_response_buffer.len()),
            None => panic!("NativeIdentityServerHandshake::verify_challenge_response(): missing verify_challenge_response_callback"),
        };

        // convert enum to Option<bool>
        match challenge_response_result {
            GoslingChallengeResponseResult::Valid => Some(true),
            GoslingChallengeResponseResult::Invalid => Some(false),
            GoslingChallengeResponseResult::Pending => None,
        }
    }

    fn poll_result(&mut self) -> Option<IdentityHandshakeResult> {
        // poll for verification result
        let challenge_response_result = match self.poll_challenge_response_result_callback {
            Some(poll_challenge_response_result_callback) => poll_challenge_response_result_callback(self.handshake_handle),
            None => panic!("NativeIdentityServerHandshake::poll_result(): missing poll_challenge_response_result_callback"),
        };

        // convert enum to Option<IdentityHandshakeResult>
        match challenge_response_result {
            GoslingChallengeResponseResult::Valid => Some(IdentityHandshakeResult::VerifyChallengeResponse(true)),
            GoslingChallengeResponseResult::Invalid => Some(IdentityHandshakeResult::VerifyChallengeResponse(false)),
            GoslingChallengeResponseResult::Pending => None,
        }
    }
}

define_registry!{NativeIdentityServerHandshake, ObjectTypes::IdentityServerHandshake}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_init(
    out_server_handshake: *mut *mut GoslingIdentityServerHandshake,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!out_server_handshake.is_null(), "gosling_identity_server_handshake_init(): out_server_handshake must not be null");
        let handle = get_native_identity_server_handshake_registry().insert(Default::default());
        unsafe {*out_server_handshake = handle as *mut GoslingIdentityServerHandshake };

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_started_callback(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakeStartedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_started_callback(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_started_callback(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_started_callback(): server_handshake is invalid"),
        };
        server_handshake.started_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_endpoint_supported_callback(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakeEndpointSupportedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_endpoint_supported_callback(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_endpoint_supported_callback(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_endpoint_supported_callback(): client_handshake is invalid"),
        };
        server_handshake.endpoint_supported_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_challenge_size_callack(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakeChallengeSizeCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_challenge_size_callack(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_challenge_size_callack(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_challenge_size_callack(): client_handshake is invalid"),
        };
        server_handshake.challenge_size_callack = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_build_challenge_callback(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakeBuildChallengeCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_build_challenge_callback(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_build_challenge_callback(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_build_challenge_callback(): client_handshake is invalid"),
        };
        server_handshake.build_challenge_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_verify_challenge_response_callback(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakeVerifyChallengeResponseCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_verify_challenge_response_callback(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_verify_challenge_response_callback(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_verify_challenge_response_callback(): client_handshake is invalid"),
        };
        server_handshake.verify_challenge_response_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_identity_server_handshake_set_poll_challenge_response_result_callback(
    server_handshake: *mut GoslingIdentityServerHandshake,
    callback: GoslingIdentityServerHandshakePollChallengeResponseResultCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!server_handshake.is_null(), "gosling_identity_server_handshake_set_poll_challenge_response_result_callback(): server_handshake must not be null");
        ensure!(!(callback as *const c_void).is_null(), "gosling_identity_server_handshake_set_poll_challenge_response_result_callback(): callback must not be null");

        let mut native_identity_server_registry = get_native_identity_server_handshake_registry();
        let mut server_handshake = match native_identity_server_registry.get_mut(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_identity_server_handshake_set_poll_challenge_response_result_callback(): client_handshake is invalid"),
        };
        server_handshake.poll_challenge_response_result_callback = Some(callback);
        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_init(
    // out context
    out_context: *mut *mut GoslingContext,
    tor_working_directory: *const c_char,
    tor_working_directory_length: usize,
    identity_port: u16,
    endpoint_port: u16,
    identity_private_key: *const GoslingEd25519PrivateKey,
    blocked_clients: *const *const GoslingV3OnionServiceId,
    blocked_clients_count: usize,

    client_handshake: *mut GoslingIdentityClientHandshake,
    server_handshake: *mut GoslingIdentityServerHandshake,

    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        // validate params

        // data
        ensure!(!out_context.is_null(), "gosling_context_init(): out_context must not be null");
        ensure!(!tor_working_directory.is_null(), "gosling_context_init(): tor_working_directory must not be null");
        ensure!(tor_working_directory_length > 0, "gosling_context_init(): tor_working_directory_length must not be 0");
        ensure!(identity_port != 0u16, "gosling_context_init(): identity_port must not be 0");
        ensure!(endpoint_port != 0u16, "gosling_context_init(): endpoint_port must not be 0");
        ensure!(!identity_private_key.is_null(), "gosling_context_init(): identity_private_key may not be null");
        ensure!((blocked_clients.is_null() && blocked_clients_count == 0) || (!blocked_clients.is_null() && blocked_clients_count > 0), "gosling_context_init(): blocked_clients must not be null or blocked_clients_count must not be 0");
        ensure!(!client_handshake.is_null(), "gosling_context_init(): client_handshake must not be null");
        ensure!(!server_handshake.is_null(), "gosling_context_init(): server_handshake must not be null");

        // tor working dir
        let tor_working_directory = unsafe { std::slice::from_raw_parts(tor_working_directory as *const u8, tor_working_directory_length) };
        let tor_working_directory = std::str::from_utf8(tor_working_directory)?;
        let tor_working_directory = Path::new(tor_working_directory);

        // get our identity key
        let ed25519_private_key_registry = get_ed25519_private_key_registry();
        let identity_private_key = match ed25519_private_key_registry.get(identity_private_key as usize) {
            Some(identity_private_key) => identity_private_key,
            None => bail!("gosling_context_init(): identity_private_key is invalid"),
        };

        let blocked_clients: HashSet<V3OnionServiceId> = if blocked_clients.is_null() {
            Default::default()
        } else {
            // construct set of blocked clients
            let blocked_clients_slice = unsafe { std::slice::from_raw_parts(blocked_clients, blocked_clients_count) };
            let v3_onion_service_id_registry = get_v3_onion_service_id_registry();
            let mut blocked_clients : HashSet<V3OnionServiceId> = Default::default();
            for blocked_client in blocked_clients_slice.iter() {
                let blocked_client = match v3_onion_service_id_registry.get(*blocked_client as usize) {
                    Some(blocked_client) => blocked_client,
                    None => bail!("gosling_context_init(): invalid gosling_v3_onion_service_id in blocked_clients ( {:?} )", *blocked_client as *const c_void),
                };
                blocked_clients.insert(blocked_client.clone());
            }
            blocked_clients
        };

        // get client handshake from registry
        let client_handshake = match get_native_identity_client_handshake_registry().remove(client_handshake as usize) {
            Some(client_handshake) => client_handshake,
            None => bail!("gosling_context_init(): client_handshake is invalid"),
        };
        ensure!(client_handshake.started_callback.is_some(), "gosling_context_init(): client_handshake missing started_callback");
        ensure!(client_handshake.challenge_response_size_callback.is_some(), "gosling_context_init(): client_handshake missing challenge_response_size_callback");
        ensure!(client_handshake.build_challenge_response_callback.is_some(), "gosling_context_init(): client_handshake missing build_challenge_response_callback");

        // get server handshake from registry
        let server_handshake = match get_native_identity_server_handshake_registry().remove(server_handshake as usize) {
            Some(server_handshake) => server_handshake,
            None => bail!("gosling_context_init(): server_handshake is invalid"),
        };
        ensure!(server_handshake.started_callback.is_some(), "gosling_context_init(): server_handshake missing started_callback");
        ensure!(server_handshake.endpoint_supported_callback.is_some(), "gosling_context_init(): server_handshake missing endpoint_supported_callback");
        ensure!(server_handshake.challenge_size_callack.is_some(), "gosling_context_init(): server_handshake missing challenge_size_callack");
        ensure!(server_handshake.build_challenge_callback.is_some(), "gosling_context_init(): server_handshake missing build_challenge_callback");
        ensure!(server_handshake.verify_challenge_response_callback.is_some(), "gosling_context_init(): server_handshake missing verify_challenge_response_callback");
        ensure!(server_handshake.poll_challenge_response_result_callback.is_some(), "gosling_context_init(): server_handshake missing poll_challenge_response_result_callback");


        // construct context
        let context = Context::new(
            client_handshake,
            server_handshake,
            tor_working_directory,
            identity_port,
            endpoint_port,
            identity_private_key.clone(),
            blocked_clients)?;

        let handle = get_context_tuple_registry().insert((context, Default::default()));
        unsafe {*out_context = handle as *mut GoslingContext };

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_bootstrap_tor(
    context: *mut GoslingContext,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {

        ensure!(!context.is_null(), "gosling_context_bootstrap_tor(): context must not be null");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_bootstrap_tor(): context is invalid");
            }
        };
        context.0.bootstrap()
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_start_identity_server(
    context: *mut GoslingContext,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_start_identity_server(): context must not be null");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_start_identity_server(): context is invalid");
            }
        };
        context.0.start_identity_server()
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_stop_identity_server(
    context: *mut GoslingContext,
    error: *mut *mut GoslingError) ->() {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_stop_identity_server(): context must not be null");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_stop_identity_server(): context is invalid");
            }
        };
        context.0.stop_identity_server()
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_start_endpoint_server(
    context: *mut GoslingContext,
    endpoint_private_key: *const GoslingEd25519PrivateKey,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    client_identity: *const GoslingV3OnionServiceId,
    client_auth_public_key: *const GoslingX25519PublicKey,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_start_endpoint_server(): context must not be null");
        ensure!(!endpoint_private_key.is_null(), "gosling_context_start_endpoint_server(): endpoint_private_key must not be null");
        ensure!(!endpoint_name.is_null(), "gosling_context_start_endpoint_server(): endpoint_name must not be null");
        ensure!(endpoint_name_length > 0, "gosling_context_start_endpoint_server(): endpoint_name_length must not be 0");
        ensure!(!client_identity.is_null(), "gosling_context_start_endpoint_server(): client_identity must not be null");
        ensure!(!client_auth_public_key.is_null(), "gosling_context_start_endpoint_server(): client_auth_public_key must not be null");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_start_endpoint_server(): context is invalid");
            }
        };

        let endpoint_name = unsafe { std::slice::from_raw_parts(endpoint_name as *const u8, endpoint_name_length) };
        let endpoint_name = std::str::from_utf8(endpoint_name)?.to_string();
        ensure!(endpoint_name.is_ascii(), "gosling_context_start_endpoint_server(): endpoint_name must be an ascii string");

        let ed25519_private_key_registry = get_ed25519_private_key_registry();
        let endpoint_private_key = match ed25519_private_key_registry.get(endpoint_private_key as usize) {
            Some(ed25519_private_key) => ed25519_private_key,
            None => {
                bail!("gosling_context_start_endpoint_server(): endpoint_private_key is invalid");
            }
        };

        let v3_onion_service_id_registry = get_v3_onion_service_id_registry();
        let client_identity = match v3_onion_service_id_registry.get(client_identity as usize) {
            Some(v3_onion_service_id) => v3_onion_service_id,
            None => {
                bail!("gosling_context_start_endpoint_server(): client_identity is invalid");
            }
        };

        let x25519_public_key_registry = get_x25519_public_key_registry();
        let client_auth_public_key = match x25519_public_key_registry.get(client_auth_public_key as usize) {
            Some(x25519_public_key) => x25519_public_key,
            None => {
                bail!("gosling_context_start_endpoint_server(): client_auth_public_key is invalid");
            }
        };

        context.0.start_endpoint_server(endpoint_private_key.clone(), endpoint_name, client_identity.clone(), client_auth_public_key.clone())
    });

}

#[no_mangle]
pub extern "C" fn gosling_context_stop_endpoint_server(
    context: *mut GoslingContext,
    endpoint_private_key: *const GoslingEd25519PrivateKey,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_stop_endpoint_server(): context must not be null");
        ensure!(!endpoint_private_key.is_null(), "gosling_context_stop_endpoint_server(): endpoint_private_key must not be null");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_stop_endpoint_server(): context is invalid");
            }
        };

        let ed25519_private_key_registry = get_ed25519_private_key_registry();
        let endpoint_private_key = match ed25519_private_key_registry.get(endpoint_private_key as usize) {
            Some(ed25519_private_key) => ed25519_private_key,
            None => {
                bail!("gosling_context_stop_endpoint_server(): endpoint_private_key is invalid");
            }
        };

        let endpoint_identity = V3OnionServiceId::from_private_key(endpoint_private_key);
        context.0.stop_endpoint_server(endpoint_identity)
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_request_remote_endpoint(
    context: *mut GoslingContext,
    identity_service_id: *const GoslingV3OnionServiceId,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_request_remote_endpoint(): context must not be null");
        ensure!(!identity_service_id.is_null(), "gosling_context_request_remote_endpoint(): identity_service_id must not be null");
        ensure!(!endpoint_name.is_null(), "gosling_context_request_remote_endpoint(): endpoint_name must not be null");
        ensure!(endpoint_name_length > 0, "gosling_context_request_remote_endpoint(): endpoint_name_length must not be 0");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_request_remote_endpoint(): context is invalid");
            }
        };

        let v3_onion_service_id_registry = get_v3_onion_service_id_registry();
        let identity_service_id = match v3_onion_service_id_registry.get(identity_service_id as usize) {
            Some(v3_onion_service_id) => v3_onion_service_id,
            None => {
                bail!("gosling_context_request_remote_endpoint(): identity_service_id is invalid");
            }
        };

        let endpoint_name = unsafe { std::slice::from_raw_parts(endpoint_name as *const u8, endpoint_name_length) };
        let endpoint_name = std::str::from_utf8(endpoint_name)?.to_string();
        ensure!(endpoint_name.is_ascii(), "gosling_context_request_remote_endpoint(): endpoint_name must be an ascii string");

        context.0.request_remote_endpoint(identity_service_id.clone(), &endpoint_name)
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_open_endpoint_channel(
    context: *mut GoslingContext,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    client_auth_private_key: *const GoslingX25519PrivateKey,
    channel_name: *const c_char,
    channel_name_length: usize,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        ensure!(!context.is_null(), "gosling_context_open_endpoint_channel(): context must not be null");
        ensure!(!endpoint_service_id.is_null(), "gosling_context_open_endpoint_channel(): endpoint_service_id must not be null");
        ensure!(!client_auth_private_key.is_null(), "gosling_context_open_endpoint_channel(): client_auth_private_key must not be null");
        ensure!(!channel_name.is_null(), "gosling_context_open_endpoint_channel(): channel_name must not be null");
        ensure!(channel_name_length > 0, "gosling_context_open_endpoint_channel(): channel_name_length must not be 0");

        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_open_endpoint_channel(): context is invalid");
            }
        };

        let v3_onion_service_id_registry = get_v3_onion_service_id_registry();
        let endpoint_service_id = match v3_onion_service_id_registry.get(endpoint_service_id as usize) {
            Some(v3_onion_service_id) => v3_onion_service_id,
            None => {
                bail!("gosling_context_open_endpoint_channel(): endpoint_service_id is invalid");
            }
        };

        let x25519_private_key_registry = get_x25519_private_key_registry();
        let client_auth_private_key = match x25519_private_key_registry.get(client_auth_private_key as usize) {
            Some(x25519_private_key) => x25519_private_key,
            None => {
                bail!("gosling_context_open_endpoint_channel(): client_auth_private_key is invalid");
            }
        };

        let channel_name = unsafe { std::slice::from_raw_parts(channel_name as *const u8, channel_name_length) };
        let channel_name = std::str::from_utf8(channel_name)?.to_string();
        ensure!(channel_name.is_ascii(), "gosling_context_open_endpoint_channel(): channel_name must be an ascii string");

        context.0.open_endpoint_channel(endpoint_service_id.clone(), client_auth_private_key.clone(), &channel_name)
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_poll_events(
    context: *mut GoslingContext,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {

        // we need to scope the context registry explicitly here
        // in case our callbacks want to call any gosling functions
        // to avoid deadlock (since a mutex is held while the context_tuple_registry
        // is accesible)
        let (mut context_events, callbacks) = {
            let mut context_tuple_registry = get_context_tuple_registry();
            let mut context = match context_tuple_registry.get_mut(context as usize) {
                Some(context) => context,
                None => {
                    bail!("gosling_context_poll_events(): context is invalid");
                }
            };
            let mut context_events = context.0.update()?;
            let callbacks = context.1.clone();
            (context_events, callbacks)
        };

        for event in context_events.drain(..) {
            match event {
                ContextEvent::TorBootstrapStatusReceived{progress, tag, summary} => {
                    if let Some(callback) = callbacks.tor_bootstrap_status_received_callback {
                        let tag0 = CString::new(tag.as_str()).expect("gosling_context_poll_events(): unexpected null byte in bootstrap status tag");
                        let summary0 = CString::new(summary.as_str()).expect("gosling_context_poll_events(): unexpected null byte in bootstrap status summary");
                        callback(context, progress, tag0.as_ptr(), tag.len(), summary0.as_ptr(), summary.len());
                    }
                },
                ContextEvent::TorBootstrapCompleted => {
                    if let Some(callback) = callbacks.tor_bootstrap_completed_callback {
                        callback(context);
                    }
                },
                ContextEvent::TorLogReceived{line} => {
                    if let Some(callback) = callbacks.tor_log_received_callback {
                        let line0 = CString::new(line.as_str()).expect("gosling_context_poll_events(): unexpected null byte in tor log line");
                        callback(context, line0.as_ptr(), line.len());
                    }
                },
                ContextEvent::IdentityServerPublished => {
                    if let Some(callback) = callbacks.identity_server_published_callbck {
                        callback(context);
                    }
                },
                ContextEvent::EndpointServerPublished{
                    endpoint_service_id,
                    endpoint_name} => {
                    if let Some(callback) = callbacks.endpoint_server_published_callback {
                        let endpoint_service_id = {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            v3_onion_service_id_registry.insert(endpoint_service_id)
                        };
                        let endpoint_name0 = CString::new(endpoint_name.as_str()).expect("gosling_context_poll_events(): unexpected null byte in endpoint name");

                        callback(context, endpoint_service_id as *const GoslingV3OnionServiceId, endpoint_name0.as_ptr(), endpoint_name.len());

                        // cleanup
                        get_v3_onion_service_id_registry().remove(endpoint_service_id);
                    }
                },
                ContextEvent::EndpointClientRequestCompleted{
                    identity_service_id,
                    endpoint_service_id,
                    endpoint_name,
                    client_auth_private_key} => {
                    if let Some(callback) = callbacks.endpoint_client_request_completed_callback {
                        let (identity_service_id, endpoint_service_id) = {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            let identity_service_id = v3_onion_service_id_registry.insert(identity_service_id);
                            let endpoint_service_id = v3_onion_service_id_registry.insert(endpoint_service_id);
                            (identity_service_id, endpoint_service_id)
                        };

                        let endpoint_name0 =  CString::new(endpoint_name.as_str()).expect("gosling_context_poll_events(): unexpected null byte in endpoint name");

                        let client_auth_private_key = {
                            let mut x25519_private_key_registry = get_x25519_private_key_registry();
                            x25519_private_key_registry.insert(client_auth_private_key)
                        };

                        callback(context, identity_service_id as *const GoslingV3OnionServiceId, endpoint_service_id as *const GoslingV3OnionServiceId, endpoint_name0.as_ptr(), endpoint_name.len(), client_auth_private_key as *const GoslingX25519PrivateKey);

                        {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            v3_onion_service_id_registry.remove(identity_service_id);
                            v3_onion_service_id_registry.remove(endpoint_service_id);
                        }

                        // cleanup
                        get_x25519_private_key_registry().remove(client_auth_private_key);
                    }
                },
                ContextEvent::EndpointServerRequestCompleted{
                    endpoint_private_key,
                    endpoint_name,
                    client_service_id,
                    client_auth_public_key} => {
                    if let Some(callback) = callbacks.endpoint_server_request_completed_callback {
                        let endpoint_private_key = {
                            let mut ed25519_private_key_registry = get_ed25519_private_key_registry();
                            ed25519_private_key_registry.insert(endpoint_private_key)
                        };

                        let endpoint_name0 =  CString::new(endpoint_name.as_str()).expect("gosling_context_poll_events(): unexpected null byte in endpoint name");

                        let client_service_id = {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            v3_onion_service_id_registry.insert(client_service_id)
                        };

                        let client_auth_public_key = {
                            let mut x25519_public_key_registry = get_x25519_public_key_registry();
                            x25519_public_key_registry.insert(client_auth_public_key)
                        };

                        callback(context, endpoint_private_key as *const GoslingEd25519PrivateKey, endpoint_name0.as_ptr(), endpoint_name.len(), client_service_id as *const GoslingV3OnionServiceId, client_auth_public_key as *const GoslingX25519PublicKey);

                        // cleanup
                        get_ed25519_private_key_registry().remove(endpoint_private_key);
                        get_v3_onion_service_id_registry().remove(client_service_id);
                        get_x25519_public_key_registry().remove(client_auth_public_key);
                    }
                },
                ContextEvent::EndpointClientChannelRequestCompleted{
                    endpoint_service_id,
                    channel_name,
                    stream} => {
                    if let Some(callback) = callbacks.endpoint_client_channel_request_completed_callback {
                        let endpoint_service_id = {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            v3_onion_service_id_registry.insert(endpoint_service_id)
                        };
                        let channel_name0 = CString::new(channel_name.as_str()).expect("gosling_context_poll_events(): unexpected null byte in channel name");

                        #[cfg(any(target_os = "linux", target_os = "macos"))]
                        let stream = stream.into_raw_fd();
                        #[cfg(target_os = "windows")]
                        let stream = stream.into_raw_socket();

                        callback(context, endpoint_service_id as *const GoslingV3OnionServiceId, channel_name0.as_ptr(), channel_name.len(), stream);

                        // cleanup
                        get_v3_onion_service_id_registry().remove(endpoint_service_id);
                    }
                },
                ContextEvent::EndpointServerChannelRequestCompleted{
                    endpoint_service_id,
                    client_service_id,
                    channel_name,
                    stream} => {
                    if let Some(callback) = callbacks.endpoint_server_channel_request_completed_callback {
                        let (endpoint_service_id, client_service_id) = {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            let endpoint_service_id = v3_onion_service_id_registry.insert(endpoint_service_id);
                            let client_service_id = v3_onion_service_id_registry.insert(client_service_id);
                            (endpoint_service_id, client_service_id)
                        };

                        let channel_name0 = CString::new(channel_name.as_str()).expect("gosling_context_poll_events(): unexpected null byte in channel name");

                        #[cfg(any(target_os = "linux", target_os = "macos"))]
                        let stream = stream.into_raw_fd();
                        #[cfg(target_os = "windows")]
                        let stream = stream.into_raw_socket();

                        callback(context,  endpoint_service_id as *const GoslingV3OnionServiceId, client_service_id as *const GoslingV3OnionServiceId, channel_name0.as_ptr(), channel_name.len(), stream);

                        // cleanup
                        {
                            let mut v3_onion_service_id_registry = get_v3_onion_service_id_registry();
                            v3_onion_service_id_registry.remove(endpoint_service_id);
                            v3_onion_service_id_registry.remove(client_service_id);
                        }
                    }
                },
            }
        }

        Ok(())
    });
}

///
/// Event Callbacks
///

pub type GoslingTorBootstrapStatusReceivedCallback = extern fn(
    context: *mut GoslingContext,
    progress: u32,
    tag: *const c_char,
    tag_length: usize,
    summary: *const c_char,
    summary_length: usize) -> ();

pub type GoslingTorBootstrapCompletedCallback = extern fn(
    context: *mut GoslingContext) -> ();

pub type GoslingTorLogRecieved = extern fn(
    context: *mut GoslingContext,
    line: *const c_char,
    line_length: usize) -> ();

pub type GoslingIdentityServerPublishedCallback = extern fn(
    context: *mut GoslingContext) -> ();

pub type GoslingEndpointServerPublishedCallback = extern fn(
    context: *mut GoslingContext,
    enpdoint_service_id: *const GoslingV3OnionServiceId,
    endpoint_name: *const c_char,
    endpoint_name_length: usize) -> ();

pub type GoslingEndpointClientRequestCompletedCallback = extern fn (
    context: *mut GoslingContext,
    identity_service_id: *const GoslingV3OnionServiceId,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    client_auth_private_key: *const GoslingX25519PrivateKey) -> ();

pub type GoslingEndpointServerRequestCompletedCallback = extern fn (
    context: *mut GoslingContext,
    endpoint_private_key: *const GoslingEd25519PrivateKey,
    endpoint_name: *const c_char,
    endpoint_name_length: usize,
    client_service_id: *const GoslingV3OnionServiceId,
    client_auth_public_key: *const GoslingX25519PublicKey) -> ();

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub type GoslingEndpointClientChannelRequestCompletedCallback = extern fn(
    context: *mut GoslingContext,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    channel_name: *const c_char,
    channel_name_length: usize,
    stream: RawFd);

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub type GoslingEndpointServerChannelRequestCompletedCallback = extern fn(
    context: *mut GoslingContext,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    client_service_id: *const GoslingV3OnionServiceId,
    channel_name: *const c_char,
    channel_name_length: usize,
    stream: RawFd);

#[cfg(target_os = "windows")]
pub type GoslingEndpointClientChannelRequestCompletedCallback = extern fn(
    context: *mut GoslingContext,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    channel_name: *const c_char,
    channel_name_length: usize,
    stream: RawSocket);

#[cfg(target_os = "windows")]
pub type GoslingEndpointServerChannelRequestCompletedCallback = extern fn(
    context: *mut GoslingContext,
    endpoint_service_id: *const GoslingV3OnionServiceId,
    client_service_id: *const GoslingV3OnionServiceId,
    channel_name: *const c_char,
    channel_name_length: usize,
    stream: RawSocket);

#[derive(Default, Clone)]
pub struct EventCallbacks {
    tor_bootstrap_status_received_callback: Option<GoslingTorBootstrapStatusReceivedCallback>,
    tor_bootstrap_completed_callback: Option<GoslingTorBootstrapCompletedCallback>,
    tor_log_received_callback: Option<GoslingTorLogRecieved>,
    identity_server_published_callbck: Option<GoslingIdentityServerPublishedCallback>,
    endpoint_server_published_callback: Option<GoslingEndpointServerPublishedCallback>,
    endpoint_client_request_completed_callback: Option<GoslingEndpointClientRequestCompletedCallback>,
    endpoint_server_request_completed_callback: Option<GoslingEndpointServerRequestCompletedCallback>,
    endpoint_client_channel_request_completed_callback: Option<GoslingEndpointClientChannelRequestCompletedCallback>,
    endpoint_server_channel_request_completed_callback: Option<GoslingEndpointServerChannelRequestCompletedCallback>,
}

/// Setters for Event Callbacks

#[no_mangle]
pub extern "C" fn gosling_context_set_tor_bootstrap_status_received_callback(
    context: *mut GoslingContext,
    callback: GoslingTorBootstrapStatusReceivedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_tor_bootstrap_status_received_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.tor_bootstrap_status_received_callback = None;
        } else {
            context.1.tor_bootstrap_status_received_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_tor_bootstrap_completed_callback(
    context: *mut GoslingContext,
    callback: GoslingTorBootstrapCompletedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_tor_bootstrap_completed_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.tor_bootstrap_completed_callback = None;
        } else {
            context.1.tor_bootstrap_completed_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_tor_log_received_callback(
    context: *mut GoslingContext,
    callback: GoslingTorLogRecieved,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_tor_log_received_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.tor_log_received_callback = None;
        } else {
            context.1.tor_log_received_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_identity_server_published_callback(
    context: *mut GoslingContext,
    callback: GoslingIdentityServerPublishedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_identity_server_published_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.identity_server_published_callbck = None;
        } else {
            context.1.identity_server_published_callbck = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_endpoint_server_published_callback(
    context: *mut GoslingContext,
    callback: GoslingEndpointServerPublishedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_endpoint_server_published_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.endpoint_server_published_callback = None;
        } else {
            context.1.endpoint_server_published_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_endpoint_client_request_completed_callback(
    context: *mut GoslingContext,
    callback: GoslingEndpointClientRequestCompletedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_endpoint_client_request_completed_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.endpoint_client_request_completed_callback = None;
        } else {
            context.1.endpoint_client_request_completed_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_endpoint_server_request_completed_callback(
    context: *mut GoslingContext,
    callback: GoslingEndpointServerRequestCompletedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_endpoint_server_request_completed_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.endpoint_server_request_completed_callback = None;
        } else {
            context.1.endpoint_server_request_completed_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_endpoint_client_channel_request_completed_callback(
    context: *mut GoslingContext,
    callback: GoslingEndpointClientChannelRequestCompletedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_endpoint_client_channel_request_completed_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.endpoint_client_channel_request_completed_callback = None;
        } else {
            context.1.endpoint_client_channel_request_completed_callback = Some(callback);
        }

        Ok(())
    });
}

#[no_mangle]
pub extern "C" fn gosling_context_set_endpoint_server_channel_request_completed_callback(
    context: *mut GoslingContext,
    callback: GoslingEndpointServerChannelRequestCompletedCallback,
    error: *mut *mut GoslingError) -> () {
    translate_failures((), error, || -> Result<()> {
        let mut context_tuple_registry = get_context_tuple_registry();
        let mut context = match context_tuple_registry.get_mut(context as usize) {
            Some(context) => context,
            None => {
                bail!("gosling_context_set_endpoint_server_channel_request_completed_callback(): context is invalid");
            }
        };

        if (callback as *const c_void).is_null() {
            context.1.endpoint_server_channel_request_completed_callback = None;
        } else {
            context.1.endpoint_server_channel_request_completed_callback = Some(callback);
        }

        Ok(())
    });
}
