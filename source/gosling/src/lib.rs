// ABI BREAKING

// INTERNAL

// TODO: prune/update FFI callback args
// TODO: translate_failures should be able to handle error'ing when library not yet init'd
// TODO: FFI functions should catch all errors and return nice error messages, no '?' or unwrap()'s here
// TODO: implement a customizable logger for internal debug logging and purge printlns throughout the library
// TODO: print some warning when starting a server with callbacks missing
// TODO: add more ensure_*! rules to error and simplify some of our error handling
// TODO: APIs for identity server to set the endpoint private key/service id rather than generating new
// TODO: APIs for identity cleint to set the endpint client auth key rather than generating new

// some internal functions take a lot of args but thats ok
#![allow(clippy::too_many_arguments)]

mod error;
mod ffi;
mod gosling;
mod honk_rpc;
mod object_registry;
#[cfg(test)]
mod test_utils;
mod tor_controller;
mod tor_crypto;
