//! `VM/src/ludata.h` constants (the allocation code lives in `lfunc.rs`).

use crate::lobject::LUA_UTAG_LIMIT;

/// Userdata with an inline destructor (`lua_newuserdatadtor`).
pub const UTAG_IDTOR: usize = LUA_UTAG_LIMIT;
/// `newproxy` userdata.
pub const UTAG_PROXY: usize = LUA_UTAG_LIMIT + 1;
pub const UTAG_INTERNAL_LIMIT: usize = UTAG_PROXY + 1;
