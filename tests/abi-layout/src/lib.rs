//! The guest32 layouts, re-exported from the crate that defines them.
//!
//! `vela-abi-guest32` is the definition of the 68k wire format: this crate
//! used to carry a copy of it while `amiga/abi-guest32/src/lib.rs` was a
//! one-line placeholder. The implementation now lives where it belongs, and
//! this crate is a thin re-export so the assertions in
//! `tests/abi-layout/tests/` keep exercising the real definition instead of
//! a copy that could drift away from it.
pub use vela_abi_guest32::*;
