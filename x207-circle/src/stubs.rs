//! Interface stubs of CT-068: the P0 methods and views whose implementation items are not done yet,
//! with the final signatures of spec sections 4, 5, and 14, so `abi.json` can be frozen first.
//!
//! The lane that implements a method deletes its stub in the same commit and adds the real method
//! (anywhere in the crate, with the same name, argument names, and types). A stub only panics
//! `ERR_NOT_IMPLEMENTED`; it never changes state. Argument names are part of the ABI, so they keep
//! their spec names even when unused.
//!
//! Every P0 method is implemented now, so no stub is left.
