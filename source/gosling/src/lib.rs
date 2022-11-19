// ABI BREAKING

// INTERNAL

// TODO: translate_failures should be able to handle error'ing when library not yet init'd
// TODO: FFI functions should catch all errors and return nice error messages, no '?' or unwrap()'s here
// TODO: implement a customizable logger for internal debug logging and purge printlns throughout the library
// TODO: print some warning when starting a server with callbacks missing
// TODO: review all `unwrap() calls
// TODO: add more ensure_*! rules to error and simplify some of our error handling

// remove this once refactoring is complete
#![allow(unused_variables)]
// some internal functions take a lot of args but thats ok
#![allow(clippy::too_many_arguments)]

mod error;
mod ffi;
mod tor_crypto;
mod object_registry;
mod tor_controller;
mod honk_rpc;
mod gosling;
#[cfg(test)]
mod test_utils;
