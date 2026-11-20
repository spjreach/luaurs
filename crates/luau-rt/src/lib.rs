#![feature(explicit_tail_calls)]
#![feature(core_intrinsics)]
#![allow(internal_features)]
#![allow(incomplete_features)]
//! `luau-rt`: Rust port of the Luau VM runtime (`VM/src`), following the
//! C++ implementation structure (`lua_State`, `CallInfo`, stack-based C API,
//! incremental GC, continuation-based protected calls).

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, clippy::missing_safety_doc)]

pub mod jit;
pub mod lapi;
pub mod laux;
pub mod lbuiltins;
pub mod lclass;
pub mod ldebug;
pub mod ldo;
pub mod lfunc;
pub mod lgc;
pub mod libm;
pub mod lmathlib;
pub mod lmem;
pub mod lnumutils;
pub mod lobject;
pub mod los;
pub mod lstate;
pub mod lstring;
pub mod ltable;
pub mod ltm;
pub mod ludata;
pub mod lvmexecute;
pub mod lvmload;
pub mod lvmutils;
pub mod lbaselib;
pub mod lcfmt;
pub mod lstrlib;
pub mod ltablib;
pub mod lcorolib;
pub mod lbitlib;
pub mod lutf8lib;
pub mod ldblib;
pub mod lbuflib;
pub mod lveclib;
pub mod lregex;
pub mod lintlib;
pub mod ltasklib;
pub mod lsignal;
pub mod linit;
pub mod codegen;
pub use luau_common::fastalloc;
