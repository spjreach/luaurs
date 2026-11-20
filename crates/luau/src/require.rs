//! `require` for the CLI (port of `CLI/src/ReplRequirer.cpp`): modules are
//! found on the file system with [`VfsNavigator`] (relative paths, `@self`,
//! aliases from `.luaurc` / `.config.luau`), compiled, and run on their own
//! sandboxed thread.

use luau_require::{ConfigStatus, NavigateResult, Requirer};
use luau_rt::lapi::*;
use luau_rt::laux::*;
use luau_rt::ldo::lua_resume;
use luau_rt::linit::luaL_sandboxthread;
use luau_rt::lobject::*;
use luau_rt::lvmload::luau_load;

use crate::vfs::{is_absolute_path, is_file, VfsNavigator};

#[derive(Default)]
struct ReplRequirer {
    vfs: VfsNavigator,
}

impl Requirer for ReplRequirer {
    unsafe fn is_require_allowed(&mut self, _L: *mut lua_State, requirer_chunkname: &str) -> bool {
        requirer_chunkname == "=stdin" || requirer_chunkname.starts_with('@')
    }

    unsafe fn reset(&mut self, _L: *mut lua_State, requirer_chunkname: &str) -> NavigateResult {
        if requirer_chunkname == "=stdin" {
            self.vfs.reset_to_stdin()
        } else if let Some(path) = requirer_chunkname.strip_prefix('@') {
            self.vfs.reset_to_path(path)
        } else {
            NavigateResult::NotFound
        }
    }

    unsafe fn jump_to_alias(&mut self, _L: *mut lua_State, path: &str) -> NavigateResult {
        if !is_absolute_path(path) {
            return NavigateResult::NotFound;
        }
        self.vfs.reset_to_path(path)
    }

    unsafe fn to_parent(&mut self, _L: *mut lua_State) -> NavigateResult {
        self.vfs.to_parent()
    }

    unsafe fn to_child(&mut self, _L: *mut lua_State, name: &str) -> NavigateResult {
        self.vfs.to_child(name)
    }

    unsafe fn is_module_present(&mut self, _L: *mut lua_State) -> bool {
        is_file(self.vfs.get_file_path())
    }

    unsafe fn get_chunkname(&mut self, _L: *mut lua_State) -> Option<String> {
        Some(format!("@{}", self.vfs.get_file_path()))
    }

    unsafe fn get_loadname(&mut self, _L: *mut lua_State) -> Option<String> {
        Some(self.vfs.get_absolute_file_path().to_string())
    }

    unsafe fn get_cache_key(&mut self, _L: *mut lua_State) -> Option<String> {
        Some(self.vfs.get_absolute_file_path().to_string())
    }

    unsafe fn get_config_status(&mut self, _L: *mut lua_State) -> ConfigStatus {
        self.vfs.get_config_status()
    }

    unsafe fn get_config(&mut self, _L: *mut lua_State) -> Option<String> {
        self.vfs.get_config()
    }

    unsafe fn load(&mut self, L: *mut lua_State, _path: &str, chunkname: &str, loadname: &str) -> i32 {
        // the module runs in a new thread, created on the main thread so
        // that it does not inherit the environment of L
        let GL = lua_mainthread(L);
        let ML = lua_newthread(GL);
        lua_xmove(GL, L, 1);
        luaL_sandboxthread(ML);

        // owned buffers are released before any Luau error is raised
        let status = match std::fs::read(loadname) {
            Ok(contents) => {
                let bytecode = luau_compiler::compile_source(&contents, crate::copts());
                Some(luau_load(ML, chunkname.as_bytes(), &bytecode, 0))
            }
            Err(_) => None,
        };
        let Some(status) = status else {
            luaL_where(L, 1);
            lua_pushstring(L, &format!("could not read file '{}'", loadname));
            lua_concat(L, 2);
            lua_error(L);
        };

        if status == 0 {
            if lua_usesexport(ML, -1) != 0 {
                luau_require::luarequire_createplaceholder(L);
            }
            crate::maybe_codegen(ML);
            let status = lua_resume(ML, L, 0);
            if status == LUA_OK {
                if lua_gettop(ML) != 1 {
                    luaL_error(L, "module must return a single value");
                }
            } else if status == LUA_YIELD {
                luaL_error(L, "module can not yield");
            } else if lua_isstring(ML, -1) == 0 {
                luaL_error(L, "unknown error while running module");
            } else {
                luaL_where(L, 1);
                lua_pushstring(L, "error while running module: ");
                lua_xmove(ML, L, 1);
                lua_concat(L, 3);
                lua_error(L);
            }
        }

        // add the module's result (or its load error) to L's stack and
        // remove the thread
        lua_xmove(ML, L, 1);
        lua_remove(L, -2);
        1
    }
}

pub unsafe fn luaopen_require(L: *mut lua_State) {
    luau_require::luaopen_require(L, Box::new(ReplRequirer::default()));
}
