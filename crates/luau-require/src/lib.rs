//! Port of Luau's `Require` module: the require-by-string runtime library
//! (`Require.cpp`, `RequireImpl.cpp`, `Navigation.cpp`) and its navigation
//! algorithm (`navigator`).
//!
//! The embedder implements [`Requirer`] (the Rust counterpart of
//! `luarequire_Configuration`) and installs `require` with
//! [`luaopen_require`].

#![allow(non_snake_case)]

pub mod navigator;

use std::time::Duration;

use luau_rt::lapi::*;
use luau_rt::laux::*;
use luau_rt::ldebug::{lua_Debug, lua_getinfo};
use luau_rt::ldo::lua_yield;
use luau_rt::lobject::*;

pub use navigator::{ConfigBehavior, ConfigStatus, NavigateResult, NavigationContext, Navigator};

const REGISTERED_CACHE_TABLE_KEY: &str = "_REGISTEREDMODULES";
const REQUIRED_CACHE_TABLE_KEY: &str = "_MODULES";
const CYCLIC_PLACEHOLDER_METATABLE_KEY: &str = "_CYCLIC_PLACEHOLDER_METATABLE";
const CYCLIC_PLACEHOLDER_PROVIDED_KEY: &str = "_CYCLIC_PLACEHOLDER_PROVIDED";

/// What the embedder provides to `require` (`luarequire_Configuration`).
/// Navigation state lives in the implementation.
pub trait Requirer {
    /// Whether requires are permitted from the given chunkname.
    unsafe fn is_require_allowed(&mut self, L: *mut lua_State, requirer_chunkname: &str) -> bool;
    /// Resets the internal state to point at the requirer module.
    unsafe fn reset(&mut self, L: *mut lua_State, requirer_chunkname: &str) -> NavigateResult;
    /// Resets the internal state to point at an aliased module, given its
    /// exact path from a configuration file.
    unsafe fn jump_to_alias(&mut self, L: *mut lua_State, path: &str) -> NavigateResult;
    unsafe fn to_alias_override(&mut self, _L: *mut lua_State, _alias_unprefixed: &str) -> NavigateResult {
        NavigateResult::NotFound
    }
    unsafe fn to_alias_fallback(&mut self, _L: *mut lua_State, _alias_unprefixed: &str) -> NavigateResult {
        NavigateResult::NotFound
    }
    unsafe fn to_parent(&mut self, L: *mut lua_State) -> NavigateResult;
    unsafe fn to_child(&mut self, L: *mut lua_State, name: &str) -> NavigateResult;
    /// Whether a module is present at the current location.
    unsafe fn is_module_present(&mut self, L: *mut lua_State) -> bool;
    unsafe fn get_chunkname(&mut self, L: *mut lua_State) -> Option<String>;
    unsafe fn get_loadname(&mut self, L: *mut lua_State) -> Option<String>;
    unsafe fn get_cache_key(&mut self, L: *mut lua_State) -> Option<String>;
    unsafe fn get_config_status(&mut self, L: *mut lua_State) -> ConfigStatus;
    /// True when aliases are looked up with `get_alias` instead of parsing
    /// the configuration returned by `get_config`.
    fn uses_get_alias(&self) -> bool {
        false
    }
    unsafe fn get_alias(&mut self, _L: *mut lua_State, _alias: &str) -> Option<String> {
        None
    }
    unsafe fn get_config(&mut self, L: *mut lua_State) -> Option<String>;
    /// Time limit in milliseconds for `.config.luau` scripts (negative: none).
    unsafe fn get_luau_config_timeout(&mut self, _L: *mut lua_State) -> i32 {
        2000
    }
    /// Loads the module: pushes its result and returns the number of results
    /// (-1 to yield).
    unsafe fn load(&mut self, L: *mut lua_State, path: &str, chunkname: &str, loadname: &str) -> i32;
}

struct RuntimeNavigationContext<'a> {
    req: &'a mut dyn Requirer,
    L: *mut lua_State,
    requirer_chunkname: &'a str,
}

impl NavigationContext for RuntimeNavigationContext<'_> {
    fn reset_to_requirer(&mut self) -> NavigateResult {
        unsafe { self.req.reset(self.L, self.requirer_chunkname) }
    }
    fn jump_to_alias(&mut self, path: &str) -> NavigateResult {
        unsafe { self.req.jump_to_alias(self.L, path) }
    }
    fn to_alias_override(&mut self, alias: &str) -> NavigateResult {
        unsafe { self.req.to_alias_override(self.L, alias) }
    }
    fn to_alias_fallback(&mut self, alias: &str) -> NavigateResult {
        unsafe { self.req.to_alias_fallback(self.L, alias) }
    }
    fn to_parent(&mut self) -> NavigateResult {
        unsafe { self.req.to_parent(self.L) }
    }
    fn to_child(&mut self, component: &str) -> NavigateResult {
        unsafe { self.req.to_child(self.L, component) }
    }
    fn get_config_status(&self) -> ConfigStatus {
        // the requirer keeps its own state; querying does not change it
        let req = self.req as *const dyn Requirer as *mut dyn Requirer;
        unsafe { (*req).get_config_status(self.L) }
    }
    fn get_config_behavior(&self) -> ConfigBehavior {
        if self.req.uses_get_alias() {
            ConfigBehavior::GetAlias
        } else {
            ConfigBehavior::GetConfig
        }
    }
    fn get_alias(&mut self, alias: &str) -> Option<String> {
        unsafe { self.req.get_alias(self.L, alias) }
    }
    fn get_config(&self) -> Option<String> {
        let req = self.req as *const dyn Requirer as *mut dyn Requirer;
        unsafe { (*req).get_config(self.L) }
    }
    fn luau_config_timeout(&self) -> Option<Duration> {
        let req = self.req as *const dyn Requirer as *mut dyn Requirer;
        let ms = unsafe { (*req).get_luau_config_timeout(self.L) };
        if ms < 0 {
            None
        } else {
            Some(Duration::from_millis(ms as u64))
        }
    }
}

enum Resolved {
    /// the cached result has been pushed
    Cached,
    ModuleRead { chunkname: String, loadname: String, cache_key: String },
    Error(String),
}

unsafe fn is_cached(L: *mut lua_State, key: &str) -> bool {
    luaL_findtable(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY, 1);
    lua_getfield(L, -1, key);
    let cached = !lua_isnil(L, -1);
    if cached && lua_istable(L, -1) && lua_getmetatable(L, -1) == 1 {
        // a placeholder for a module that is still loading (a cyclic
        // require): remember that it was handed out
        lua_getfield(L, LUA_REGISTRYINDEX, CYCLIC_PLACEHOLDER_METATABLE_KEY);
        if lua_rawequal(L, -1, -2) == 1 {
            luaL_findtable(L, LUA_REGISTRYINDEX, CYCLIC_PLACEHOLDER_PROVIDED_KEY, 1);
            lua_pushboolean(L, 1);
            lua_setfield(L, -2, key);
            lua_pop(L, 1);
        }
        lua_pop(L, 2); // both metatables
    }
    lua_pop(L, 2);
    cached
}

unsafe fn cyclic_error(L: *mut lua_State, verb: &str) -> ! {
    let key = lua_tostring(L, 2).map(|k| String::from_utf8_lossy(k).into_owned()).unwrap_or_else(|| "unknown".to_string());
    luaL_where(L, 1);
    lua_pushstring(L, &format!("Cannot {} the exported field '{}' because it has a cyclic dependency on its requiring module", verb, key));
    drop(key);
    lua_concat(L, 2);
    lua_error(L)
}

unsafe fn cyclic_dependency_index_error(L: *mut lua_State) -> i32 {
    cyclic_error(L, "access")
}

unsafe fn cyclic_dependency_newindex_error(L: *mut lua_State) -> i32 {
    cyclic_error(L, "set")
}

unsafe fn push_cyclic_placeholder_metatable(L: *mut lua_State) {
    lua_getfield(L, LUA_REGISTRYINDEX, CYCLIC_PLACEHOLDER_METATABLE_KEY);
    if !lua_isnil(L, -1) {
        return;
    }
    lua_pop(L, 1);
    lua_newtable(L);
    lua_pushcfunction(L, cyclic_dependency_index_error, Some("CyclicDependencyIndexError"));
    lua_setfield(L, -2, "__index");
    lua_pushcfunction(L, cyclic_dependency_newindex_error, Some("CyclicDependencyNewIndexError"));
    lua_setfield(L, -2, "__newindex");
    lua_pushstring(L, "The metatable is locked");
    lua_setfield(L, -2, "__metatable");
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, CYCLIC_PLACEHOLDER_METATABLE_KEY);
}

/// `luarequire_createplaceholder`: called from `Requirer::load` (with the
/// require call's stack intact) for modules that export values, before the
/// module runs; requires of the module while it is still loading get a
/// locked placeholder that is filled in when loading finishes.
pub unsafe fn luarequire_createplaceholder(L: *mut lua_State) {
    let cache_key = String::from_utf8_lossy(luaL_checkstring(L, 2)).into_owned();
    lua_newtable(L);
    push_cyclic_placeholder_metatable(L);
    lua_setmetatable(L, -2);
    lua_setreadonly(L, -1, 1);
    luaL_findtable(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY, 1);
    lua_pushvalue(L, -2);
    lua_setfield(L, -2, &cache_key);
    lua_pop(L, 2);
}

unsafe fn populate_placeholder(L: *mut lua_State, placeholder_idx: i32, result_idx: i32) {
    let placeholder_idx = lua_absindex(L, placeholder_idx);
    let result_idx = lua_absindex(L, result_idx);
    lua_setreadonly(L, placeholder_idx, 0);
    let mut iter = 0;
    loop {
        iter = lua_rawiter(L, result_idx, iter);
        if iter == -1 {
            break;
        }
        lua_rawset(L, placeholder_idx);
    }
    if lua_getmetatable(L, result_idx) == 0 {
        lua_pushnil(L);
    }
    lua_setmetatable(L, placeholder_idx);
    lua_setreadonly(L, placeholder_idx, 1);
}

unsafe fn resolve_require(req: &mut dyn Requirer, L: *mut lua_State, requirer_chunkname: &str, path: &str) -> Resolved {
    if !req.is_require_allowed(L, requirer_chunkname) {
        return Resolved::Error("require is not supported in this context".to_string());
    }
    {
        let mut ctx = RuntimeNavigationContext { req: &mut *req, L, requirer_chunkname };
        let mut navigator = Navigator::new(&mut ctx);
        if let Err(e) = navigator.navigate(path) {
            return Resolved::Error(format!("error requiring module \"{}\": {}", path, e));
        }
    }
    if !req.is_module_present(L) {
        return Resolved::Error("no module present at resolved path".to_string());
    }
    let Some(cache_key) = req.get_cache_key(L) else {
        return Resolved::Error("could not get cache key for module".to_string());
    };
    if is_cached(L, &cache_key) {
        lua_getfield(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY);
        lua_getfield(L, -1, &cache_key);
        lua_remove(L, -2);
        return Resolved::Cached;
    }
    let Some(chunkname) = req.get_chunkname(L) else {
        return Resolved::Error("could not get chunkname for module".to_string());
    };
    let Some(loadname) = req.get_loadname(L) else {
        return Resolved::Error("could not get loadname for module".to_string());
    };
    Resolved::ModuleRead { chunkname, loadname, cache_key }
}

unsafe fn check_registered_modules(L: *mut lua_State, path: &str) -> bool {
    luaL_findtable(L, LUA_REGISTRYINDEX, REGISTERED_CACHE_TABLE_KEY, 1);
    let lower = path.to_ascii_lowercase();
    lua_getfield(L, -1, &lower);
    if lua_isnil(L, -1) {
        lua_pop(L, 2);
        return false;
    }
    lua_remove(L, -2);
    true
}

const REQUIRE_STACK_VALUES: i32 = 4;

/// Continuation of `require`: caches and returns the module's result.
unsafe fn lua_requirecont(L: *mut lua_State, _status: i32) -> i32 {
    let num_results = lua_gettop(L) - REQUIRE_STACK_VALUES;
    if num_results > 1 {
        luaL_error(L, "module must return a single value");
    }
    if num_results == 1 {
        let cache_key = String::from_utf8_lossy(luaL_checkstring(L, 2)).into_owned();
        let result_idx = REQUIRE_STACK_VALUES + 1;

        luaL_findtable(L, LUA_REGISTRYINDEX, CYCLIC_PLACEHOLDER_PROVIDED_KEY, 1);
        lua_getfield(L, -1, &cache_key);
        let was_provided = lua_toboolean(L, -1) != 0;
        lua_pop(L, 1);
        lua_pushnil(L);
        lua_setfield(L, -2, &cache_key);
        lua_pop(L, 1);

        luaL_findtable(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY, 1);
        if was_provided {
            // somebody holds the placeholder: it becomes the module's value
            lua_getfield(L, -1, &cache_key);
            populate_placeholder(L, -1, result_idx);
            lua_replace(L, result_idx);
            lua_pop(L, 1);
        } else {
            lua_pushvalue(L, result_idx);
            lua_setfield(L, -2, &cache_key);
            lua_pop(L, 1);
        }
    }
    num_results
}

unsafe fn requirer_of<'a>(L: *mut lua_State) -> Option<&'a mut dyn Requirer> {
    let p = lua_tolightuserdata(L, lua_upvalueindex(1)) as *mut Box<dyn Requirer>;
    if p.is_null() {
        None
    } else {
        Some(&mut **p)
    }
}

unsafe fn lua_requireinternal(L: *mut lua_State, requirer_chunkname: &str) -> i32 {
    lua_settop(L, 1);
    let Some(req) = requirer_of(L) else { luaL_error(L, "unable to find require configuration") };
    let path = String::from_utf8_lossy(luaL_checkstring(L, 1)).into_owned();

    if check_registered_modules(L, &path) {
        return 1;
    }

    let (chunkname, loadname) = match resolve_require(req, L, requirer_chunkname, &path) {
        Resolved::Cached => return 1,
        Resolved::Error(e) => {
            // the message moves to the Lua stack before the error is raised
            lua_pushstring(L, &e);
            drop(e);
            drop(path);
            lua_error(L);
        }
        Resolved::ModuleRead { chunkname, loadname, cache_key } => {
            lua_pushstring(L, &cache_key);
            lua_pushstring(L, &chunkname);
            lua_pushstring(L, &loadname);
            (chunkname, loadname)
        }
    };

    let num_results = req.load(L, &path, &chunkname, &loadname);
    if num_results == -1 {
        if lua_gettop(L) != REQUIRE_STACK_VALUES {
            luaL_error(L, "stack cannot be modified when require yields");
        }
        return lua_yield(L, 0);
    }
    lua_requirecont(L, LUA_OK)
}

/// `require(path)`: the requirer is the innermost Luau function on the stack.
unsafe fn lua_require(L: *mut lua_State) -> i32 {
    let mut ar = lua_Debug::default();
    let mut level = 1;
    loop {
        if lua_getinfo(L, level, b"s", &mut ar) == 0 {
            luaL_error(L, "require is not supported in this context");
        }
        level += 1;
        if !ar.what.starts_with('C') {
            break;
        }
    }
    let source = String::from_utf8_lossy(ar.source.as_deref().unwrap_or(b"")).into_owned();
    drop(ar);
    lua_requireinternal(L, &source)
}

/// `proxyrequire(path, requirerChunkname)`
unsafe fn lua_proxyrequire(L: *mut lua_State) -> i32 {
    let requirer_chunkname = String::from_utf8_lossy(luaL_checkstring(L, 2)).into_owned();
    lua_requireinternal(L, &requirer_chunkname)
}

unsafe fn register_module_impl(L: *mut lua_State) -> i32 {
    if lua_gettop(L) != 2 {
        luaL_error(L, "expected 2 arguments: aliased require path and desired result");
    }
    let path = luaL_checkstring(L, 1);
    if path.first() != Some(&b'@') {
        luaL_argerror(L, 1, "path must begin with '@'");
    }
    let lower = String::from_utf8_lossy(path).to_ascii_lowercase();
    lua_pushstring(L, &lower);
    lua_replace(L, 1);
    luaL_findtable(L, LUA_REGISTRYINDEX, REGISTERED_CACHE_TABLE_KEY, 1);
    lua_insert(L, 1);
    lua_settable(L, 1);
    lua_pop(L, 1);
    0
}

unsafe fn clear_cache_entry(L: *mut lua_State) -> i32 {
    let key = String::from_utf8_lossy(luaL_checkstring(L, 1)).into_owned();
    luaL_findtable(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY, 1);
    lua_pushnil(L);
    lua_setfield(L, -2, &key);
    lua_pop(L, 1);
    0
}

unsafe fn clear_cache(L: *mut lua_State) -> i32 {
    lua_newtable(L);
    lua_setfield(L, LUA_REGISTRYINDEX, REQUIRED_CACHE_TABLE_KEY);
    0
}

/// `luarequire_pushrequire`: pushes a `require` closure bound to `requirer`
/// (which lives for the rest of the process).
pub unsafe fn luarequire_pushrequire(L: *mut lua_State, requirer: Box<dyn Requirer>) {
    let ctx = Box::into_raw(Box::new(requirer));
    lua_pushlightuserdata(L, ctx as *mut u8);
    lua_pushcclosurek(L, lua_require, Some("require"), 1, Some(lua_requirecont));
}

/// `luarequire_pushproxyrequire`: like `require`, but the requirer's
/// chunkname is passed as a second argument.
pub unsafe fn luarequire_pushproxyrequire(L: *mut lua_State, requirer: Box<dyn Requirer>) {
    let ctx = Box::into_raw(Box::new(requirer));
    lua_pushlightuserdata(L, ctx as *mut u8);
    lua_pushcclosurek(L, lua_proxyrequire, Some("proxyrequire"), 1, Some(lua_requirecont));
}

/// `luaopen_require`: sets the global `require`.
pub unsafe fn luaopen_require(L: *mut lua_State, requirer: Box<dyn Requirer>) {
    luarequire_pushrequire(L, requirer);
    lua_setglobal(L, "require");
}

/// `luarequire_registermodule`: (path, result) on the stack; `require(path)`
/// returns `result` from then on.
pub unsafe fn luarequire_registermodule(L: *mut lua_State) -> i32 {
    register_module_impl(L)
}

/// `luarequire_clearcacheentry`: (cache key) on the stack.
pub unsafe fn luarequire_clearcacheentry(L: *mut lua_State) -> i32 {
    clear_cache_entry(L)
}

/// `luarequire_clearcache`
pub unsafe fn luarequire_clearcache(L: *mut lua_State) -> i32 {
    clear_cache(L)
}
