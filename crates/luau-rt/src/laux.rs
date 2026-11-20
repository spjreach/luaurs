//! Port of `VM/src/laux.cpp`: auxiliary library (argument checks, errors,
//! string buffers, registration helpers).
//!
//! `luaL_Strbuf` is a Rust byte vector: unlike the C++ buffer it never boxes
//! storage on the Lua stack, so callers' stack layout is unaffected.

use crate::lapi::*;
use crate::ldebug::{lua_Debug, lua_getinfo};
use crate::lnumutils::*;
use crate::lobject::*;
use crate::ltm::luaT_objtypename;

pub type luaL_Reg = (&'static str, lua_CFunction);

#[inline]
fn abs_index(L: *mut lua_State, i: i32) -> i32 {
    if i > 0 || i <= LUA_REGISTRYINDEX {
        i
    } else {
        unsafe { lua_gettop(L) + i + 1 }
    }
}

unsafe fn currfuncname(L: *mut lua_State) -> Option<Vec<u8>> {
    if (*L).ci <= (*L).base_ci {
        return None;
    }
    let cl = curr_func(L);
    if (*cl).isC == 0 {
        return None;
    }
    let dn = Closure::c(cl).debugname;
    if dn.is_null() {
        return None;
    }
    let name = sbytes(dn);
    if name == b"__namecall" {
        if (*L).namecall.is_null() {
            None
        } else {
            Some(sbytes((*L).namecall).to_vec())
        }
    } else {
        Some(name.to_vec())
    }
}

#[cold]
pub unsafe fn luaL_argerror(L: *mut lua_State, narg: i32, extramsg: &str) -> ! {
    let msg = match currfuncname(L) {
        Some(f) => format!("invalid argument #{} to '{}' ({})", narg, String::from_utf8_lossy(&f), extramsg),
        None => format!("invalid argument #{} ({})", narg, extramsg),
    };
    luaL_error_owned(L, msg)
}

#[cold]
pub unsafe fn luaL_typeerror(L: *mut lua_State, narg: i32, tname: &str) -> ! {
    // message built in its own scope: nothing may be left owning memory
    // when the error longjmps out of this frame
    let msg = {
        let fname = currfuncname(L).map(|f| String::from_utf8_lossy(&f).into_owned());
        let obj = luaA_toobject(L, narg);
        if !obj.is_null() {
            let got = luaT_objtypename(L, obj);
            match fname {
                Some(f) => format!("invalid argument #{} to '{}' ({} expected, got {})", narg, f, tname, got),
                None => format!("invalid argument #{} ({} expected, got {})", narg, tname, got),
            }
        } else {
            match fname {
                Some(f) => format!("missing argument #{} to '{}' ({} expected)", narg, f, tname),
                None => format!("missing argument #{} ({} expected)", narg, tname),
            }
        }
    };
    luaL_error_owned(L, msg)
}

#[cold]
unsafe fn tag_error(L: *mut lua_State, narg: i32, tag: i32) -> ! {
    luaL_typeerror(L, narg, lua_typename(tag))
}

pub unsafe fn luaL_where(L: *mut lua_State, level: i32) {
    let mut ar = lua_Debug::default();
    if lua_getinfo(L, level, b"sl", &mut ar) != 0 && ar.currentline > 0 {
        let mut s = ar.short_src.clone();
        s.extend_from_slice(format!(":{}: ", ar.currentline).as_bytes());
        s.truncate(LUA_BUFFERSIZE - 1);
        lua_pushlstring(L, &s);
        return;
    }
    lua_rawcheckstack(L, 1);
    lua_pushlstring(L, b"");
}

/// Position prefix as bytes (for building messages without the stack).
pub unsafe fn luaL_where_bytes(L: *mut lua_State, level: i32) -> Vec<u8> {
    let mut ar = lua_Debug::default();
    if lua_getinfo(L, level, b"sl", &mut ar) != 0 && ar.currentline > 0 {
        let mut s = ar.short_src.clone();
        s.extend_from_slice(format!(":{}: ", ar.currentline).as_bytes());
        return s;
    }
    Vec::new()
}

#[cold]
pub unsafe fn luaL_error(L: *mut lua_State, msg: &str) -> ! {
    luaL_error_bytes(L, msg.as_bytes())
}

/// `luaL_error` with an owned message, freed before throwing (see
/// `luaG_runerror_owned`).
#[cold]
#[inline(never)]
pub unsafe fn luaL_error_owned(L: *mut lua_State, msg: String) -> ! {
    luaL_where(L, 1);
    lua_pushlstring(L, &msg.as_bytes()[..msg.len().min(LUA_BUFFERSIZE - 1)]);
    drop(msg);
    lua_concat(L, 2);
    lua_error(L)
}

#[cold]
pub unsafe fn luaL_error_bytes(L: *mut lua_State, msg: &[u8]) -> ! {
    luaL_where(L, 1);
    lua_pushlstring(L, &msg[..msg.len().min(LUA_BUFFERSIZE - 1)]);
    lua_concat(L, 2);
    lua_error(L)
}

pub unsafe fn luaL_checkoption(L: *mut lua_State, narg: i32, def: Option<&str>, lst: &[&str]) -> i32 {
    let name = match def {
        Some(d) => luaL_optlstring(L, narg, d.as_bytes()),
        None => luaL_checklstring(L, narg),
    };
    for (i, s) in lst.iter().enumerate() {
        if s.as_bytes() == name {
            return i as i32;
        }
    }
    // like C++ (lua_pushfstring): the message lives on the Lua stack, so
    // nothing owned is left behind when the error longjmps away
    lua_pushstring(L, &format!("invalid option '{}'", String::from_utf8_lossy(name)));
    let msg: &str = std::str::from_utf8_unchecked(lua_tolstring(L, -1).unwrap_or(b""));
    luaL_argerror(L, narg, msg)
}

pub unsafe fn luaL_newmetatable(L: *mut lua_State, tname: &str) -> i32 {
    lua_getfield(L, LUA_REGISTRYINDEX, tname);
    if !lua_isnil(L, -1) {
        return 0;
    }
    lua_pop(L, 1);
    lua_newtable(L);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, tname);
    1
}

pub unsafe fn luaL_getmetatable(L: *mut lua_State, tname: &str) -> i32 {
    lua_getfield(L, LUA_REGISTRYINDEX, tname)
}

pub unsafe fn luaL_checkudata(L: *mut lua_State, ud: i32, tname: &str) -> *mut u8 {
    let p = lua_touserdata(L, ud);
    if !p.is_null() && lua_getmetatable(L, ud) != 0 {
        lua_getfield(L, LUA_REGISTRYINDEX, tname);
        if lua_rawequal(L, -1, -2) != 0 {
            lua_pop(L, 2);
            return p;
        }
    }
    luaL_typeerror(L, ud, tname)
}

pub unsafe fn luaL_checkudatatagged(L: *mut lua_State, ud: i32, tag: i32) -> *mut u8 {
    let p = lua_touserdatatagged(L, ud, tag);
    if !p.is_null() {
        return p;
    }
    let tname = lua_getuserdataname(L, tag);
    luaL_typeerror(L, ud, &String::from_utf8_lossy(&tname))
}

pub unsafe fn luaL_checkbuffer(L: *mut lua_State, narg: i32) -> (*mut u8, usize) {
    let mut len = 0usize;
    let b = lua_tobuffer(L, narg, Some(&mut len));
    if b.is_null() {
        tag_error(L, narg, LUA_TBUFFER);
    }
    (b, len)
}

pub unsafe fn luaL_checkstack(L: *mut lua_State, space: i32, mes: &str) {
    if lua_checkstack(L, space) == 0 {
        luaL_error_owned(L, format!("stack overflow ({})", mes));
    }
}

pub unsafe fn luaL_checktype(L: *mut lua_State, narg: i32, t: i32) {
    if lua_type(L, narg) != t {
        tag_error(L, narg, t);
    }
}

pub unsafe fn luaL_checkany(L: *mut lua_State, narg: i32) {
    if lua_type(L, narg) == LUA_TNONE {
        luaL_error_owned(L, format!("missing argument #{}", narg));
    }
}

pub unsafe fn luaL_checklstring<'a>(L: *mut lua_State, narg: i32) -> &'a [u8] {
    match lua_tolstring(L, narg) {
        Some(s) => s,
        None => tag_error(L, narg, LUA_TSTRING),
    }
}

#[inline]
pub unsafe fn luaL_checkstring<'a>(L: *mut lua_State, narg: i32) -> &'a [u8] {
    luaL_checklstring(L, narg)
}

pub unsafe fn luaL_optlstring<'a>(L: *mut lua_State, narg: i32, def: &'a [u8]) -> &'a [u8] {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checklstring(L, narg)
    }
}

pub unsafe fn luaL_checknumber(L: *mut lua_State, narg: i32) -> f64 {
    let mut isnum = 0;
    let d = lua_tonumberx(L, narg, Some(&mut isnum));
    if isnum == 0 {
        tag_error(L, narg, LUA_TNUMBER);
    }
    d
}

pub unsafe fn luaL_optnumber(L: *mut lua_State, narg: i32, def: f64) -> f64 {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checknumber(L, narg)
    }
}

pub unsafe fn luaL_checkboolean(L: *mut lua_State, narg: i32) -> i32 {
    if !lua_isboolean(L, narg) {
        tag_error(L, narg, LUA_TBOOLEAN);
    }
    lua_toboolean(L, narg)
}

pub unsafe fn luaL_optboolean(L: *mut lua_State, narg: i32, def: i32) -> i32 {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checkboolean(L, narg)
    }
}

pub unsafe fn luaL_checkinteger(L: *mut lua_State, narg: i32) -> i32 {
    let mut isnum = 0;
    let d = lua_tointegerx(L, narg, Some(&mut isnum));
    if isnum == 0 {
        tag_error(L, narg, LUA_TNUMBER);
    }
    d
}

pub unsafe fn luaL_optinteger(L: *mut lua_State, narg: i32, def: i32) -> i32 {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checkinteger(L, narg)
    }
}

pub unsafe fn luaL_checkinteger64(L: *mut lua_State, narg: i32) -> i64 {
    if lua_type(L, narg) != LUA_TINTEGER {
        tag_error(L, narg, LUA_TINTEGER);
    }
    lua_tointeger64(L, narg, None)
}

pub unsafe fn luaL_optinteger64(L: *mut lua_State, narg: i32, def: i64) -> i64 {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checkinteger64(L, narg)
    }
}

pub unsafe fn luaL_checkunsigned(L: *mut lua_State, narg: i32) -> u32 {
    let mut isnum = 0;
    let d = lua_tounsignedx(L, narg, Some(&mut isnum));
    if isnum == 0 {
        tag_error(L, narg, LUA_TNUMBER);
    }
    d
}

pub unsafe fn luaL_optunsigned(L: *mut lua_State, narg: i32, def: u32) -> u32 {
    if lua_isnoneornil(L, narg) {
        def
    } else {
        luaL_checkunsigned(L, narg)
    }
}

pub unsafe fn luaL_checkvector(L: *mut lua_State, narg: i32) -> [f32; 3] {
    match lua_tovector(L, narg) {
        Some(v) => v,
        None => tag_error(L, narg, LUA_TVECTOR),
    }
}

pub unsafe fn luaL_getmetafield(L: *mut lua_State, obj: i32, event: &str) -> i32 {
    if lua_getmetatable(L, obj) == 0 {
        return 0;
    }
    lua_pushstring(L, event);
    lua_rawget(L, -2);
    if lua_isnil(L, -1) {
        lua_pop(L, 2);
        0
    } else {
        lua_remove(L, -2);
        1
    }
}

pub unsafe fn luaL_callmeta(L: *mut lua_State, obj: i32, event: &str) -> i32 {
    let obj = abs_index(L, obj);
    if luaL_getmetafield(L, obj, event) == 0 {
        return 0;
    }
    lua_pushvalue(L, obj);
    lua_call(L, 1, 1);
    1
}

pub unsafe fn luaL_register(L: *mut lua_State, libname: Option<&str>, l: &[luaL_Reg]) {
    if let Some(libname) = libname {
        let size = l.len() as i32;
        luaL_findtable(L, LUA_REGISTRYINDEX, "_LOADED", 1);
        lua_getfield(L, -1, libname);
        if !lua_istable(L, -1) {
            lua_pop(L, 1);
            if luaL_findtable(L, LUA_GLOBALSINDEX, libname, size).is_some() {
                luaL_error_owned(L, format!("name conflict for module '{}'", libname));
            }
            lua_pushvalue(L, -1);
            lua_setfield(L, -3, libname);
        }
        lua_remove(L, -2);
    }
    for (name, f) in l {
        lua_pushcfunction(L, *f, Some(name));
        lua_setfield(L, -2, name);
    }
}

pub unsafe fn luaL_findtable(L: *mut lua_State, idx: i32, fname: &str, szhint: i32) -> Option<String> {
    lua_pushvalue(L, idx);
    let parts: Vec<&str> = fname.split('.').collect();
    let n = parts.len();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == n;
        lua_pushlstring(L, part.as_bytes());
        lua_rawget(L, -2);
        if lua_isnil(L, -1) {
            lua_pop(L, 1);
            lua_createtable(L, 0, if !last { 1 } else { szhint });
            lua_pushlstring(L, part.as_bytes());
            lua_pushvalue(L, -2);
            lua_settable(L, -4);
        } else if !lua_istable(L, -1) {
            lua_pop(L, 2);
            return Some(parts[i..].join("."));
        }
        lua_remove(L, -2);
    }
    None
}

pub unsafe fn luaL_typename(L: *mut lua_State, idx: i32) -> String {
    let obj = luaA_toobject(L, idx);
    if obj.is_null() {
        "no value".to_string()
    } else {
        luaT_objtypename(L, obj).into_owned()
    }
}

pub unsafe fn luaL_traceback(L: *mut lua_State, L1: *mut lua_State, msg: Option<&[u8]>, level: i32) {
    let mut buf: Vec<u8> = Vec::new();
    if let Some(m) = msg {
        buf.extend_from_slice(m);
        buf.push(b'\n');
    }
    let mut i = level;
    loop {
        let mut ar = lua_Debug::default();
        if lua_getinfo(L1, i, b"sln", &mut ar) == 0 {
            break;
        }
        i += 1;
        if ar.what == "C" {
            continue;
        }
        if ar.source.is_some() {
            buf.extend_from_slice(&ar.short_src);
        }
        if ar.currentline > 0 {
            buf.push(b':');
            buf.extend_from_slice(ar.currentline.to_string().as_bytes());
        }
        if let Some(n) = &ar.name {
            buf.extend_from_slice(b" function ");
            buf.extend_from_slice(n);
        }
        buf.push(b'\n');
    }
    lua_pushlstring(L, &buf);
}

/// `luaL_Strbuf`.
pub struct luaL_Strbuf {
    pub L: *mut lua_State,
    pub buf: Vec<u8>,
}

impl luaL_Strbuf {
    pub fn new(L: *mut lua_State) -> Self {
        // reuse the state's spare buffer when no other builder holds it
        unsafe {
            let g = (*L).global;
            if !(*g).strbuf_ptr.is_null() {
                let buf = Vec::from_raw_parts((*g).strbuf_ptr, 0, (*g).strbuf_cap);
                (*g).strbuf_ptr = std::ptr::null_mut();
                return luaL_Strbuf { L, buf };
            }
        }
        luaL_Strbuf { L, buf: Vec::with_capacity(LUA_BUFFERSIZE) }
    }
    pub fn with_capacity(L: *mut lua_State, n: usize) -> Self {
        luaL_Strbuf { L, buf: Vec::with_capacity(n) }
    }
    #[inline]
    pub fn addchar(&mut self, c: u8) {
        self.buf.push(c);
    }
    #[inline]
    pub fn addlstring(&mut self, s: &[u8]) {
        self.buf.extend_from_slice(s);
    }
    #[inline]
    pub fn addstring(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }
    /// `luaL_addvalue`: appends the string/number at the top and pops it.
    pub unsafe fn addvalue(&mut self) {
        if let Some(s) = lua_tolstring(self.L, -1) {
            self.buf.extend_from_slice(s);
            lua_pop(self.L, 1);
        }
    }
    pub unsafe fn addvalueany(&mut self, idx: i32) {
        let L = self.L;
        match lua_type(L, idx) {
            LUA_TNIL => self.addstring("nil"),
            LUA_TBOOLEAN => self.addstring(if lua_toboolean(L, idx) != 0 { "true" } else { "false" }),
            LUA_TNUMBER => luai_num2str(&mut self.buf, lua_tonumber(L, idx)),
            LUA_TSTRING => {
                let s = lua_tolstring(L, idx).unwrap();
                self.buf.extend_from_slice(s);
            }
            LUA_TINTEGER => luai_int2str(&mut self.buf, lua_tointeger64(L, idx, None)),
            LUA_TNONE => {}
            _ => {
                luaL_tolstring(L, idx);
                self.addvalue();
            }
        }
    }
    /// `luaL_pushresult`.
    /// (takes `&mut self` so that an `errguard!`ed buffer is never moved)
    pub unsafe fn pushresult(&mut self) {
        let mut buf = std::mem::take(&mut self.buf);
        lua_pushlstring(self.L, &buf);
        // keep a moderately sized buffer as the state's spare
        let g = (*self.L).global;
        if (*g).strbuf_ptr.is_null() && (LUA_BUFFERSIZE..=65536).contains(&buf.capacity()) {
            buf.clear();
            let mut buf = std::mem::ManuallyDrop::new(buf);
            (*g).strbuf_ptr = buf.as_mut_ptr();
            (*g).strbuf_cap = buf.capacity();
        }
    }
}

pub unsafe fn luaL_tolstring<'a>(L: *mut lua_State, idx: i32) -> &'a [u8] {
    if luaL_callmeta(L, idx, "__tostring") != 0 {
        match lua_tolstring(L, -1) {
            Some(s) => return s,
            None => luaL_error(L, "'__tostring' must return a string"),
        }
    }
    match lua_type(L, idx) {
        LUA_TNIL => lua_pushlstring(L, b"nil"),
        LUA_TBOOLEAN => lua_pushstring(L, if lua_toboolean(L, idx) != 0 { "true" } else { "false" }),
        LUA_TNUMBER => {
            let mut nb = crate::lnumutils::NumBuf::new();
            let s = crate::lnumutils::luai_num2buf(&mut nb, lua_tonumber(L, idx));
            lua_pushlstring(L, s);
        }
        LUA_TVECTOR => {
            let v = lua_tovector(L, idx).unwrap();
            let mut s = Vec::new();
            for (i, c) in v.iter().enumerate() {
                if i != 0 {
                    s.extend_from_slice(b", ");
                }
                luai_num2str(&mut s, *c as f64);
            }
            lua_pushlstring(L, &s);
        }
        LUA_TSTRING => lua_pushvalue(L, idx),
        LUA_TINTEGER => {
            let mut nb = crate::lnumutils::NumBuf::new();
            let s = crate::lnumutils::luai_int2buf(&mut nb, lua_tointeger64(L, idx, None));
            lua_pushlstring(L, s);
        }
        _ => {
            let p = lua_topointer(L, idx);
            let enc = lua_encodepointer(L, p as usize);
            let s = format!("{}: 0x{:016x}", luaL_typename(L, idx), enc);
            lua_pushstring(L, &s);
        }
    }
    lua_tolstring(L, -1).unwrap()
}

pub unsafe fn luaL_ref(L: *mut lua_State, idx: i32) -> i32 {
    lua_ref(L, idx)
}
