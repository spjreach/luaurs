//! Smoke-test runner for the new runtime: compiles a script and runs it.

use luau_rt::lapi::*;
use luau_rt::lobject::*;

fn main() {
    let path = std::env::args().nth(1).expect("usage: rtrun <file>");
    let source = std::fs::read(&path).expect("read");
    let mut names = luau_ast::name_table::AstNameTable::new();
    let parsed = luau_ast::parser::parse(&source, &mut names);
    if let Some(e) = parsed.errors.first() {
        eprintln!("{}:{}: {}", path, e.location.begin.line + 1, e.message);
        std::process::exit(1);
    }
    let bc = luau_compiler::compile_parsed(&parsed, Default::default()).unwrap_or_else(|e| {
        eprintln!("{}{}", path, e);
        std::process::exit(1)
    });
    unsafe {
        let L = luau_rt::lstate::lua_newstate();
        luau_rt::lbaselib::luaopen_base(L);
        lua_pop(L, 1);
        unsafe fn os_clock(L: *mut lua_State) -> i32 {
            lua_pushnumber(L, luau_rt::los::lua_clock());
            1
        }
        lua_newtable(L);
        lua_pushcfunction(L, os_clock, Some("clock"));
        lua_setfield(L, -2, "clock");
        lua_setglobal(L, "os");
        let chunk = format!("@{}", path);
        if luau_rt::lvmload::luau_load(L, chunk.as_bytes(), &bc, 0) != 0 {
            eprintln!("{}", String::from_utf8_lossy(lua_tostring(L, -1).unwrap_or(b"?")));
            std::process::exit(1);
        }
        let status = lua_pcall(L, 0, 0, 0);
        luau_rt::lbaselib::flush_stdout();
        if status != 0 {
            eprintln!("{}", String::from_utf8_lossy(lua_tostring(L, -1).unwrap_or(b"?")));
            std::process::exit(1);
        }
        luau_rt::lstate::lua_close(L);
    }
}
