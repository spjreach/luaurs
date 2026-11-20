//! Port of `VM/include/lua.h` constants, `VM/src/lobject.h` and the data
//! structures of `VM/src/lstate.h`.
//!
//! The layout follows C++: every collectable object starts with a
//! [`GCheader`] and objects are addressed through raw pointers. One
//! deviation: the C++ VM sweeps its paged allocator, while this port links
//! every collectable object into `global_State::allgc` through
//! `GCheader::next`.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use std::ptr;

// ---------------------------------------------------------------------------
// lua.h / luaconf.h
// ---------------------------------------------------------------------------

pub const LUA_MULTRET: i32 = -1;

pub const LUAI_MAXCSTACK: i32 = 8000;
pub const LUAI_MAXCALLS: i32 = 20000;
pub const LUAI_MAXCCALLS: i32 = 200;
pub const LUA_IDSIZE: usize = 256;
pub const LUA_MINSTACK: i32 = 20;
pub const LUA_BUFFERSIZE: usize = 512;
pub const LUA_UTAG_LIMIT: usize = 128;
pub const LUA_LUTAG_LIMIT: usize = 128;
pub const LUA_MEMORY_CATEGORIES: usize = 256;
pub const LUA_MINSTRTABSIZE: i32 = 32;
pub const LUA_MAXCAPTURES: usize = 32;
pub const LUA_VECTOR_SIZE: usize = 3;
pub const LUA_EXTRA_SIZE: usize = LUA_VECTOR_SIZE - 2;

pub const LUA_REGISTRYINDEX: i32 = -LUAI_MAXCSTACK - 2000;
pub const LUA_ENVIRONINDEX: i32 = -LUAI_MAXCSTACK - 2001;
pub const LUA_GLOBALSINDEX: i32 = -LUAI_MAXCSTACK - 2002;
#[inline]
pub const fn lua_upvalueindex(i: i32) -> i32 {
    LUA_GLOBALSINDEX - i
}
#[inline]
pub const fn lua_ispseudo(i: i32) -> bool {
    i <= LUA_REGISTRYINDEX
}

// lua_Status
pub const LUA_OK: i32 = 0;
pub const LUA_YIELD: i32 = 1;
pub const LUA_ERRRUN: i32 = 2;
pub const LUA_ERRSYNTAX: i32 = 3;
pub const LUA_ERRMEM: i32 = 4;
pub const LUA_ERRERR: i32 = 5;
pub const LUA_BREAK: i32 = 6;

// lua_CoStatus
pub const LUA_CORUN: i32 = 0;
pub const LUA_COSUS: i32 = 1;
pub const LUA_CONOR: i32 = 2;
pub const LUA_COFIN: i32 = 3;
pub const LUA_COERR: i32 = 4;

// lua_Type
pub const LUA_TNONE: i32 = -1;
pub const LUA_TNIL: i32 = 0;
pub const LUA_TBOOLEAN: i32 = 1;
pub const LUA_TLIGHTUSERDATA: i32 = 2;
pub const LUA_TNUMBER: i32 = 3;
pub const LUA_TINTEGER: i32 = 4;
pub const LUA_TVECTOR: i32 = 5;
pub const LUA_TSTRING: i32 = 6;
pub const LUA_TTABLE: i32 = 7;
pub const LUA_TFUNCTION: i32 = 8;
pub const LUA_TUSERDATA: i32 = 9;
pub const LUA_TTHREAD: i32 = 10;
pub const LUA_TBUFFER: i32 = 11;
pub const LUA_TCLASS: i32 = 12;
pub const LUA_TOBJECT: i32 = 13;
pub const LUA_TDEADKEY: i32 = 14;
pub const LUA_TPROTO: i32 = 15;
pub const LUA_TUPVAL: i32 = 16;
pub const LUA_T_ALL: i32 = 17;
pub const LUA_T_COUNT: usize = LUA_TDEADKEY as usize;

// lua_gc options
pub const LUA_GCSTOP: i32 = 0;
pub const LUA_GCRESTART: i32 = 1;
pub const LUA_GCCOLLECT: i32 = 2;
pub const LUA_GCCOUNT: i32 = 3;
pub const LUA_GCCOUNTB: i32 = 4;
pub const LUA_GCISRUNNING: i32 = 5;
pub const LUA_GCSTEP: i32 = 6;
pub const LUA_GCSETGOAL: i32 = 7;
pub const LUA_GCSETSTEPMUL: i32 = 8;
pub const LUA_GCSETSTEPSIZE: i32 = 9;

pub const LUA_NOREF: i32 = -1;
pub const LUA_REFNIL: i32 = 0;

pub const LUA_MEMERRMSG: &str = "not enough memory";
pub const LUA_ERRERRMSG: &str = "error in error handling";

pub type lua_Number = f64;
pub type lua_Integer = i32;
pub type lua_Unsigned = u32;

pub type lua_CFunction = unsafe fn(L: *mut lua_State) -> i32;
pub type lua_Continuation = unsafe fn(L: *mut lua_State, status: i32) -> i32;
pub type lua_Destructor = unsafe fn(L: *mut lua_State, p: *mut u8);

pub type Instruction = u32;

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// Common header of every collectable object (`CommonHeader`) plus the
/// `allgc` list link.
#[repr(C)]
pub struct GCheader {
    pub tt: u8,
    pub marked: u8,
    pub memcat: u8,
}

/// `GCObject*`: any collectable object, addressed through its header.
pub type GCObject = GCheader;

#[repr(C)]
#[derive(Clone, Copy)]
pub union Value {
    pub gc: *mut GCObject,
    pub p: *mut u8,
    pub n: f64,
    pub b: i32,
    pub l: i64,
    pub v: [f32; 2],
}

/// `TValue`: 16 bytes like the C++ VM.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TValue {
    pub value: Value,
    pub extra: [i32; LUA_EXTRA_SIZE],
    pub tt: i32,
}

pub type StkId = *mut TValue;

unsafe impl Sync for TValue {}
unsafe impl Send for TValue {}

pub static luaO_nilobject_: TValue = TValue { value: Value { l: 0 }, extra: [0], tt: LUA_TNIL };

#[inline(always)]
pub fn luaO_nilobject() -> *const TValue {
    &luaO_nilobject_
}

impl TValue {
    pub const NIL: TValue = TValue { value: Value { l: 0 }, extra: [0], tt: LUA_TNIL };
}

// type tests
#[inline(always)]
pub unsafe fn ttype(o: *const TValue) -> i32 {
    (*o).tt
}
#[inline(always)]
pub unsafe fn ttisnil(o: *const TValue) -> bool {
    (*o).tt == LUA_TNIL
}
#[inline(always)]
pub unsafe fn ttisnumber(o: *const TValue) -> bool {
    (*o).tt == LUA_TNUMBER
}
#[inline(always)]
pub unsafe fn ttisinteger(o: *const TValue) -> bool {
    (*o).tt == LUA_TINTEGER
}
#[inline(always)]
pub unsafe fn ttisstring(o: *const TValue) -> bool {
    (*o).tt == LUA_TSTRING
}
#[inline(always)]
pub unsafe fn ttistable(o: *const TValue) -> bool {
    (*o).tt == LUA_TTABLE
}
#[inline(always)]
pub unsafe fn ttisfunction(o: *const TValue) -> bool {
    (*o).tt == LUA_TFUNCTION
}
#[inline(always)]
pub unsafe fn ttisboolean(o: *const TValue) -> bool {
    (*o).tt == LUA_TBOOLEAN
}
#[inline(always)]
pub unsafe fn ttisuserdata(o: *const TValue) -> bool {
    (*o).tt == LUA_TUSERDATA
}
#[inline(always)]
pub unsafe fn ttisthread(o: *const TValue) -> bool {
    (*o).tt == LUA_TTHREAD
}
#[inline(always)]
pub unsafe fn ttisbuffer(o: *const TValue) -> bool {
    (*o).tt == LUA_TBUFFER
}
#[inline(always)]
pub unsafe fn ttislightuserdata(o: *const TValue) -> bool {
    (*o).tt == LUA_TLIGHTUSERDATA
}
#[inline(always)]
pub unsafe fn ttisvector(o: *const TValue) -> bool {
    (*o).tt == LUA_TVECTOR
}
#[inline(always)]
pub unsafe fn ttisclass(o: *const TValue) -> bool {
    (*o).tt == LUA_TCLASS
}
#[inline(always)]
pub unsafe fn ttisobject(o: *const TValue) -> bool {
    (*o).tt == LUA_TOBJECT
}
#[inline(always)]
pub unsafe fn iscollectable(o: *const TValue) -> bool {
    (*o).tt >= LUA_TSTRING
}
#[inline(always)]
pub unsafe fn l_isfalse(o: *const TValue) -> bool {
    (*o).tt == LUA_TNIL || ((*o).tt == LUA_TBOOLEAN && (*o).value.b == 0)
}

// accessors
#[inline(always)]
pub unsafe fn gcvalue(o: *const TValue) -> *mut GCObject {
    (*o).value.gc
}
#[inline(always)]
pub unsafe fn pvalue(o: *const TValue) -> *mut u8 {
    (*o).value.p
}
#[inline(always)]
pub unsafe fn nvalue(o: *const TValue) -> f64 {
    (*o).value.n
}
#[inline(always)]
pub unsafe fn lvalue(o: *const TValue) -> i64 {
    (*o).value.l
}
#[inline(always)]
pub unsafe fn bvalue(o: *const TValue) -> i32 {
    (*o).value.b
}
/// Vector components: x, y in `value.v`, z in `extra[0]`.
#[inline(always)]
pub unsafe fn vvalue(o: *const TValue) -> *const f32 {
    ptr::addr_of!((*o).value.v) as *const f32
}
#[inline(always)]
pub unsafe fn tsvalue(o: *const TValue) -> *mut TString {
    (*o).value.gc as *mut TString
}
#[inline(always)]
pub unsafe fn uvalue(o: *const TValue) -> *mut Udata {
    (*o).value.gc as *mut Udata
}
#[inline(always)]
pub unsafe fn clvalue(o: *const TValue) -> *mut Closure {
    (*o).value.gc as *mut Closure
}
#[inline(always)]
pub unsafe fn hvalue(o: *const TValue) -> *mut LuaTable {
    (*o).value.gc as *mut LuaTable
}
#[inline(always)]
pub unsafe fn thvalue(o: *const TValue) -> *mut lua_State {
    (*o).value.gc as *mut lua_State
}
#[inline(always)]
pub unsafe fn bufvalue(o: *const TValue) -> *mut Buffer {
    (*o).value.gc as *mut Buffer
}
#[inline(always)]
pub unsafe fn upvalue(o: *const TValue) -> *mut UpVal {
    (*o).value.gc as *mut UpVal
}
#[inline(always)]
pub unsafe fn classvalue(o: *const TValue) -> *mut LuauClass {
    (*o).value.gc as *mut LuauClass
}
#[inline(always)]
pub unsafe fn objectvalue(o: *const TValue) -> *mut LuauObject {
    (*o).value.gc as *mut LuauObject
}
#[inline(always)]
pub unsafe fn lightuserdatatag(o: *const TValue) -> i32 {
    (*o).extra[0]
}

/// `LU_TAG_ITERATOR`.
pub const LU_TAG_ITERATOR: i32 = LUA_UTAG_LIMIT as i32;

// setters
//
// The second half of a TValue (`extra` + `tt`) is always written as one
// 8-byte store and values are copied as two 8-byte halves: a 16-byte copy
// (or a split narrower store) right after a setter defeats store-to-load
// forwarding, which stalls hot arithmetic/table paths.
#[inline(always)]
unsafe fn settagword(o: *mut TValue, extra: i32, tt: i32) {
    *(o as *mut u64).add(1) = (extra as u32 as u64) | ((tt as u32 as u64) << 32);
}
#[inline(always)]
pub unsafe fn setnilvalue(o: *mut TValue) {
    settagword(o, 0, LUA_TNIL);
}
#[inline(always)]
pub unsafe fn setnvalue(o: *mut TValue, x: f64) {
    (*o).value.n = x;
    settagword(o, 0, LUA_TNUMBER);
}
#[inline(always)]
pub unsafe fn setlvalue(o: *mut TValue, x: i64) {
    (*o).value.l = x;
    settagword(o, 0, LUA_TINTEGER);
}
#[inline(always)]
pub unsafe fn setvvalue(o: *mut TValue, x: f32, y: f32, z: f32) {
    (*o).value.v = [x, y];
    settagword(o, z.to_bits() as i32, LUA_TVECTOR);
}
#[inline(always)]
pub unsafe fn setpvalue(o: *mut TValue, x: *mut u8, tag: i32) {
    (*o).value.p = x;
    settagword(o, tag, LUA_TLIGHTUSERDATA);
}
#[inline(always)]
pub unsafe fn setbvalue(o: *mut TValue, x: bool) {
    *(o as *mut u64) = x as u64;
    settagword(o, 0, LUA_TBOOLEAN);
}
#[inline(always)]
pub unsafe fn setgcvalue(o: *mut TValue, x: *mut GCObject, tt: i32) {
    (*o).value.gc = x;
    settagword(o, 0, tt);
}
#[inline(always)]
pub unsafe fn setsvalue(o: *mut TValue, x: *mut TString) {
    setgcvalue(o, x as *mut GCObject, LUA_TSTRING)
}
#[inline(always)]
pub unsafe fn setuvalue(o: *mut TValue, x: *mut Udata) {
    setgcvalue(o, x as *mut GCObject, LUA_TUSERDATA)
}
#[inline(always)]
pub unsafe fn setthvalue(o: *mut TValue, x: *mut lua_State) {
    setgcvalue(o, x as *mut GCObject, LUA_TTHREAD)
}
#[inline(always)]
pub unsafe fn setbufvalue(o: *mut TValue, x: *mut Buffer) {
    setgcvalue(o, x as *mut GCObject, LUA_TBUFFER)
}
#[inline(always)]
pub unsafe fn setclvalue(o: *mut TValue, x: *mut Closure) {
    setgcvalue(o, x as *mut GCObject, LUA_TFUNCTION)
}
#[inline(always)]
pub unsafe fn sethvalue(o: *mut TValue, x: *mut LuaTable) {
    setgcvalue(o, x as *mut GCObject, LUA_TTABLE)
}
#[inline(always)]
pub unsafe fn setptvalue(o: *mut TValue, x: *mut Proto) {
    setgcvalue(o, x as *mut GCObject, LUA_TPROTO)
}
#[inline(always)]
pub unsafe fn setupvalue(o: *mut TValue, x: *mut UpVal) {
    setgcvalue(o, x as *mut GCObject, LUA_TUPVAL)
}
#[inline(always)]
pub unsafe fn setclassvalue(o: *mut TValue, x: *mut LuauClass) {
    setgcvalue(o, x as *mut GCObject, LUA_TCLASS)
}
#[inline(always)]
pub unsafe fn setobjectvalue(o: *mut TValue, x: *mut LuauObject) {
    setgcvalue(o, x as *mut GCObject, LUA_TOBJECT)
}
#[inline(always)]
pub unsafe fn setobj(o1: *mut TValue, o2: *const TValue) {
    let d = o1 as *mut u64;
    let s = o2 as *const u64;
    // volatile: keeps LLVM from merging the halves back into one 16-byte load
    let (a, b) = (std::ptr::read_volatile(s), std::ptr::read_volatile(s.add(1)));
    *d = a;
    *d.add(1) = b;
}

/// `setobj` with two separate 8-byte loads (volatile keeps LLVM from merging
/// them into one 16-byte load, which stalls on store forwarding when the
/// source was just written by a setter as value + tag word). For copying
/// values a C function has just pushed.
#[inline(always)]
pub unsafe fn setobj_fresh(o1: *mut TValue, o2: *const TValue) {
    let d = o1 as *mut u64;
    let s = o2 as *const u64;
    let (a, b) = (std::ptr::read_volatile(s), std::ptr::read_volatile(s.add(1)));
    *d = a;
    *d.add(1) = b;
}
pub use setobj as setobj2s;
pub use setobj as setobj2t;
pub use setobj as setobj2n;
pub use setobj as setobjt2t;

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct TString {
    pub hdr: GCheader,
    pub atom: i16,
    pub hash: u32,
    pub len: u32,
    /// next string in the hash table bucket
    pub snext: *mut TString,
    pub data: [u8; 1],
}

impl TString {
    #[inline(always)]
    pub unsafe fn data_ptr(ts: *const TString) -> *const u8 {
        ptr::addr_of!((*ts).data) as *const u8
    }
    #[inline(always)]
    pub unsafe fn bytes<'a>(ts: *const TString) -> &'a [u8] {
        std::slice::from_raw_parts(Self::data_ptr(ts), (*ts).len as usize)
    }
}

#[inline(always)]
pub unsafe fn getstr(ts: *const TString) -> *const u8 {
    TString::data_ptr(ts)
}
#[inline(always)]
pub unsafe fn sbytes<'a>(ts: *const TString) -> &'a [u8] {
    TString::bytes(ts)
}

#[repr(C)]
pub struct Udata {
    pub hdr: GCheader,
    pub tag: u8,
    pub len: i32,
    pub metatable: *mut LuaTable,
}

impl Udata {
    pub const DATA_OFFSET: usize = 16;
    #[inline(always)]
    pub unsafe fn data(u: *mut Udata) -> *mut u8 {
        (u as *mut u8).add(Self::DATA_OFFSET)
    }
}

#[repr(C)]
pub struct Buffer {
    pub hdr: GCheader,
    pub len: u32,
}

impl Buffer {
    pub const DATA_OFFSET: usize = 8;
    #[inline(always)]
    pub unsafe fn data(b: *mut Buffer) -> *mut u8 {
        (b as *mut u8).add(Self::DATA_OFFSET)
    }
}

pub const CALL_TARGET: i32 = 0;
pub const CONSTRUCT: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FeedbackVectorSlot {
    pub kind: i32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

pub const PEF_EXECLOCKED: u8 = 1 << 0;
pub const PEF_DEBUGLOCKED: u8 = 1 << 1;

#[repr(C)]
pub struct Proto {
    pub hdr: GCheader,
    pub nups: u8,
    pub numparams: u8,
    pub is_vararg: u8,
    pub maxstacksize: u8,
    pub flags: u8,

    pub k: *mut TValue,
    pub code: *mut Instruction,
    pub p: *mut *mut Proto,
    pub codeentry: *const Instruction,

    pub execdata: *mut u8,
    pub exectarget: usize,
    pub execcount: u32,
    pub execflags: u8,

    pub lineinfo: *mut u8,
    pub abslineinfo: *mut i32,
    pub locvars: *mut LocVar,
    pub upvalues: *mut *mut TString,
    pub source: *mut TString,

    pub debugname: *mut TString,
    pub debuginsn: *mut u8,

    pub typeinfo: *mut u8,
    pub userdata: *mut u8,

    pub gclist: *mut GCObject,

    pub sizecode: i32,
    pub sizep: i32,
    pub sizelocvars: i32,
    pub sizeupvalues: i32,
    pub sizek: i32,
    pub sizelineinfo: i32,
    pub linegaplog2: i32,
    pub linedefined: i32,
    pub bytecodeid: i32,
    pub sizetypeinfo: i32,

    pub feedbackvec: *mut FeedbackVectorSlot,
    pub feedbackvecsize: u32,
    pub funid: u32,
    pub cost: u64,
}

#[repr(C)]
pub struct LocVar {
    pub varname: *mut TString,
    pub startpc: i32,
    pub endpc: i32,
    pub reg: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UpValOpen {
    pub prev: *mut UpVal,
    pub next: *mut UpVal,
    pub threadnext: *mut UpVal,
}

#[repr(C)]
pub union UpValU {
    pub value: TValue,
    pub open: UpValOpen,
}

#[repr(C)]
pub struct UpVal {
    pub hdr: GCheader,
    pub markedopen: u8,
    pub v: *mut TValue,
    pub u: UpValU,
}

#[inline(always)]
pub unsafe fn upisopen(up: *const UpVal) -> bool {
    (*up).v as *const TValue != ptr::addr_of!((*up).u.value)
}

#[repr(C)]
pub struct CClosure {
    pub f: Option<lua_CFunction>,
    pub cont: Option<lua_Continuation>,
    pub debugname: *mut TString,
    pub upvals: [TValue; 1],
}

#[repr(C)]
pub struct LClosure {
    pub p: *mut Proto,
    pub uprefs: [TValue; 1],
}

#[repr(C)]
pub union ClosureU {
    pub c: std::mem::ManuallyDrop<CClosure>,
    pub l: std::mem::ManuallyDrop<LClosure>,
}

#[repr(C)]
pub struct Closure {
    pub hdr: GCheader,
    pub isC: u8,
    pub nupvalues: u8,
    pub stacksize: u8,
    pub preload: u8,
    pub gclist: *mut GCObject,
    pub env: *mut LuaTable,
    pub u: ClosureU,
}

impl Closure {
    #[inline(always)]
    pub unsafe fn c<'a>(cl: *mut Closure) -> &'a mut CClosure {
        &mut *(ptr::addr_of_mut!((*cl).u) as *mut CClosure)
    }
    #[inline(always)]
    pub unsafe fn l<'a>(cl: *mut Closure) -> &'a mut LClosure {
        &mut *(ptr::addr_of_mut!((*cl).u) as *mut LClosure)
    }
    #[inline(always)]
    pub unsafe fn proto(cl: *mut Closure) -> *mut Proto {
        (*(ptr::addr_of_mut!((*cl).u) as *mut LClosure)).p
    }
    #[inline(always)]
    pub unsafe fn uprefs(cl: *mut Closure) -> *mut TValue {
        ptr::addr_of_mut!((*(ptr::addr_of_mut!((*cl).u) as *mut LClosure)).uprefs) as *mut TValue
    }
    #[inline(always)]
    pub unsafe fn cupvals(cl: *mut Closure) -> *mut TValue {
        ptr::addr_of_mut!((*(ptr::addr_of_mut!((*cl).u) as *mut CClosure)).upvals) as *mut TValue
    }
}

pub const CLOSURE_HEADER: usize = std::mem::offset_of!(Closure, u);
#[inline]
pub const fn sizeCclosure(n: usize) -> usize {
    CLOSURE_HEADER + std::mem::offset_of!(CClosure, upvals) + std::mem::size_of::<TValue>() * if n == 0 { 1 } else { n }
}
#[inline]
pub const fn sizeLclosure(n: usize) -> usize {
    CLOSURE_HEADER + std::mem::offset_of!(LClosure, uprefs) + std::mem::size_of::<TValue>() * if n == 0 { 1 } else { n }
}

#[inline(always)]
pub unsafe fn iscfunction(o: *const TValue) -> bool {
    ttisfunction(o) && (*clvalue(o)).isC != 0
}
#[inline(always)]
pub unsafe fn isLfunction(o: *const TValue) -> bool {
    ttisfunction(o) && (*clvalue(o)).isC == 0
}

/// `TKey`: value + extra + 4-bit type tag + 28-bit chain offset.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TKey {
    pub value: Value,
    pub extra: [i32; LUA_EXTRA_SIZE],
    /// low 4 bits: tt; high 28 bits: next (signed)
    pub ttnext: i32,
}

impl TKey {
    #[inline(always)]
    pub fn tt(&self) -> i32 {
        self.ttnext & 0xf
    }
    #[inline(always)]
    pub fn set_tt(&mut self, tt: i32) {
        self.ttnext = (self.ttnext & !0xf) | (tt & 0xf);
    }
    #[inline(always)]
    pub fn next(&self) -> i32 {
        self.ttnext >> 4
    }
    #[inline(always)]
    pub fn set_next(&mut self, n: i32) {
        self.ttnext = (self.ttnext & 0xf) | (n << 4);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LuaNode {
    pub val: TValue,
    pub key: TKey,
}

#[inline(always)]
pub unsafe fn setnodekey(n: *mut LuaNode, obj: *const TValue) {
    (*n).key.value = (*obj).value;
    (*n).key.extra = (*obj).extra;
    (*n).key.set_tt((*obj).tt);
}

#[inline(always)]
pub unsafe fn getnodekey(obj: *mut TValue, n: *const LuaNode) {
    (*obj).value = (*n).key.value;
    (*obj).extra = (*n).key.extra;
    (*obj).tt = (*n).key.tt();
}

#[repr(C)]
pub struct LuaTable {
    pub hdr: GCheader,
    /// 1<<p means tagmethod(p) is not present
    pub tmcache: u8,
    /// bit 0 - prohibit writes to table, bit 1 - array contains a metamethod cache
    pub readonly: u8,
    pub safeenv: u8,
    pub lsizenode: u8,
    pub nodemask8: u8,
    pub sizearray: i32,
    /// `lastfree` / `aboundary` union
    pub lastfree: i32,
    pub metatable: *mut LuaTable,
    pub array: *mut TValue,
    pub node: *mut LuaNode,
    pub gclist: *mut GCObject,
}

impl LuaTable {
    #[inline(always)]
    pub fn aboundary(&self) -> i32 {
        self.lastfree
    }
    #[inline(always)]
    pub fn set_aboundary(&mut self, v: i32) {
        self.lastfree = v;
    }
}

#[repr(C)]
pub struct LuauClass {
    pub hdr: GCheader,
    pub gclist: *mut GCObject,
    pub name: *mut TString,
    pub id: u32,
    pub super_: *mut LuauClass,
    pub staticmembers: *mut TValue,
    pub privatestaticmembers: *mut TValue,
    pub memberstooffset: *mut LuaTable,
    pub offsettomember: *mut *mut TString,
    pub metatable: *mut LuaTable,
    pub instancemetatable: *mut LuaTable,
    pub numberofinstancemembers: u32,
    pub numberofmembers: u32,
    pub numberofprivatefields: u32,
    pub ownprivatefieldsstart: u32,
    pub numberofprivatestaticmembers: u8,
    pub isopen: bool,
    pub hasuserinitinchain: bool,
}

#[repr(C)]
pub struct LuauObject {
    pub hdr: GCheader,
    pub gclist: *mut GCObject,
    pub lclass: *mut LuauClass,
    pub numberofmembers: u32,
    pub members: *mut TValue,
}

#[inline(always)]
pub const fn twoto(x: i32) -> i32 {
    1 << x
}
#[inline(always)]
pub unsafe fn sizenode(t: *const LuaTable) -> i32 {
    twoto((*t).lsizenode as i32)
}
#[inline(always)]
pub unsafe fn hasmetacache(t: *const LuaTable) -> bool {
    (*t).readonly & 2 != 0
}
#[inline(always)]
pub unsafe fn getmetacache(t: *const LuaTable, event: i32) -> *mut TValue {
    (*t).array.offset(-(1 + event as isize))
}

// ---------------------------------------------------------------------------
// lstate.h
// ---------------------------------------------------------------------------

pub const EXTRA_STACK: i32 = 5;
pub const BASIC_CI_SIZE: i32 = 8;
pub const BASIC_STACK_SIZE: i32 = 2 * LUA_MINSTACK;

#[repr(C)]
pub struct stringtable {
    pub hash: *mut *mut TString,
    pub nuse: u32,
    pub size: i32,
    pub saltmult: u32,
    pub saltadd: u32,
    pub hashshift: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CallInfo {
    pub base: StkId,
    pub func: StkId,
    pub top: StkId,
    pub p: *mut Proto,
    /// `savedpc` for Lua functions, `errfunc` (as an index) for C functions
    pub savedpc: *const Instruction,
    pub nresults: i32,
    pub flags: u32,
}

impl CallInfo {
    #[inline(always)]
    pub fn errfunc(&self) -> i32 {
        self.savedpc as usize as i32
    }
    #[inline(always)]
    pub fn set_errfunc(&mut self, v: i32) {
        self.savedpc = v as usize as *const Instruction;
    }
}

pub const LUA_CALLINFO_RETURN: u32 = 1 << 0;
pub const LUA_CALLINFO_HANDLE: u32 = 1 << 1;
pub const LUA_CALLINFO_NATIVE: u32 = 1 << 2;
pub const LUA_CALLINFO_OPYIELD: u32 = 1 << 3;
pub const LUA_CALLINFO_PCALL: u32 = 1 << 4;

#[inline(always)]
pub unsafe fn ci_func(ci: *const CallInfo) -> *mut Closure {
    clvalue((*ci).func)
}
#[inline(always)]
pub unsafe fn curr_func(L: *const lua_State) -> *mut Closure {
    clvalue((*(*L).ci).func)
}
#[inline(always)]
pub unsafe fn f_isLua(ci: *const CallInfo) -> bool {
    (*ci_func(ci)).isC == 0
}
#[inline(always)]
pub unsafe fn isLua(ci: *const CallInfo) -> bool {
    ttisfunction((*ci).func) && f_isLua(ci)
}

/// Debug/host callbacks (`lua_Callbacks`).
#[derive(Default, Clone, Copy)]
pub struct lua_Callbacks {
    pub userdata: usize,
    pub interrupt: Option<unsafe fn(L: *mut lua_State, gc: i32)>,
    pub panic: Option<unsafe fn(L: *mut lua_State, errcode: i32)>,
    pub userthread: Option<unsafe fn(LP: *mut lua_State, L: *mut lua_State)>,
    pub useratom: Option<unsafe fn(L: *mut lua_State, s: *const u8, l: usize) -> i16>,
    pub debugbreak: Option<unsafe fn(L: *mut lua_State, ar: *mut crate::ldebug::lua_Debug)>,
    pub debugstep: Option<unsafe fn(L: *mut lua_State, ar: *mut crate::ldebug::lua_Debug)>,
    pub debuginterrupt: Option<unsafe fn(L: *mut lua_State, ar: *mut crate::ldebug::lua_Debug)>,
    pub debugprotectederror: Option<unsafe fn(L: *mut lua_State)>,
    pub onallocate: Option<unsafe fn(L: *mut lua_State, osize: usize, nsize: usize)>,
}

#[derive(Clone, Copy)]
pub struct GCStats {
    pub triggerterms: [i32; 32],
    pub triggertermpos: u32,
    pub triggerintegral: i32,
    pub atomicstarttotalsizebytes: usize,
    pub endtotalsizebytes: usize,
    pub heapgoalsizebytes: usize,
    pub starttimestamp: f64,
    pub atomicstarttimestamp: f64,
    pub endtimestamp: f64,
}

impl Default for GCStats {
    fn default() -> Self {
        GCStats {
            triggerterms: [0; 32],
            triggertermpos: 0,
            triggerintegral: 0,
            atomicstarttotalsizebytes: 0,
            endtotalsizebytes: 0,
            heapgoalsizebytes: 0,
            starttimestamp: 0.0,
            atomicstarttimestamp: 0.0,
            endtimestamp: 0.0,
        }
    }
}

pub const TM_N: usize = 21;

#[repr(C)]
pub struct global_State {
    pub strt: stringtable,

    pub currentwhite: u8,
    pub gcstate: u8,
    pub gcdisabled: u8,

    pub gray: *mut GCObject,
    pub grayagain: *mut GCObject,
    pub weak: *mut GCObject,

    pub GCthreshold: usize,
    pub totalbytes: usize,
    pub gcgoal: i32,
    pub gcstepmul: i32,
    pub gcstepsize: i32,

    /// pages with free blocks, per size class (non-GC / GC objects)
    pub freepages: [*mut crate::lmem::lua_Page; crate::lmem::LUA_SIZECLASSES],
    pub freegcopages: [*mut crate::lmem::lua_Page; crate::lmem::LUA_SIZECLASSES],
    /// every GC page
    pub allgcopages: *mut crate::lmem::lua_Page,
    /// sweep cursor
    pub sweepgcopage: *mut crate::lmem::lua_Page,
    /// page of the object being freed (C++ passes it to `luaM_freegco_`)
    pub freepage: *mut crate::lmem::lua_Page,

    pub mainthread: *mut lua_State,
    pub uvhead: UpVal,
    pub mt: [*mut LuaTable; LUA_T_COUNT],
    pub ttname: [*mut TString; LUA_T_COUNT],
    pub tmname: [*mut TString; TM_N],

    pub pseudotemp: TValue,
    pub registry: TValue,
    pub registryfree: i32,

    pub rngstate: u64,
    pub ptrenckey: [u64; 4],

    pub cb: lua_Callbacks,

    /// innermost active protected call (`LUA_USE_LONGJMP` error recovery)
    pub errorjmp: *mut crate::ldo::LuaJmpBuf,
    /// owned values to free if an error longjmps past them (`errguard!`)
    pub cleanups: *mut crate::ldo::ErrCleanup,

    pub udatagc: [Option<unsafe fn(L: *mut lua_State, p: *mut u8)>; LUA_UTAG_LIMIT],
    pub udatamt: [*mut LuaTable; LUA_UTAG_LIMIT],
    pub lightuserdataname: [*mut TString; LUA_LUTAG_LIMIT],

    pub memcatbytes: [usize; LUA_MEMORY_CATEGORIES],

    pub builtinPcall: *mut Closure,
    pub builtinXpcall: *mut Closure,

    pub gcstats: GCStats,
    pub lastprotoid: u32,
    pub lastclassid: u32,

    /// Largest single allocation that may succeed (`limitedRealloc` in the
    /// conformance runner); 0 = unlimited.
    pub memlimit: usize,
    /// Every allocation fails while set (`blockableRealloc`).
    pub allocblocked: bool,
    /// A spare `luaL_Strbuf` buffer (pointer, capacity; null when taken),
    /// so string building doesn't allocate on every call.
    pub strbuf_ptr: *mut u8,
    pub strbuf_cap: usize,
}

#[repr(C)]
pub struct lua_State {
    pub hdr: GCheader,
    pub status: u8,
    pub activememcat: u8,
    pub isactive: bool,
    pub singlestep: bool,

    pub top: StkId,
    pub base: StkId,
    pub global: *mut global_State,
    pub ci: *mut CallInfo,
    pub stack_last: StkId,
    pub stack: StkId,

    pub end_ci: *mut CallInfo,
    pub base_ci: *mut CallInfo,

    pub stacksize: i32,
    pub size_ci: i32,

    pub nCcalls: u16,
    pub baseCcalls: u16,

    pub cachedslot: i32,

    pub gt: *mut LuaTable,
    pub openupval: *mut UpVal,
    pub gclist: *mut GCObject,

    pub namecall: *mut TString,
    pub finalizers: *mut LuaTable,

    pub userdata: *mut u8,
}

#[inline(always)]
pub unsafe fn registry(L: *mut lua_State) -> *mut TValue {
    ptr::addr_of_mut!((*(*L).global).registry)
}

// gco casts
#[inline(always)]
pub fn obj2gco<T>(p: *mut T) -> *mut GCObject {
    p as *mut GCObject
}
#[inline(always)]
pub unsafe fn gco2ts(o: *mut GCObject) -> *mut TString {
    o as *mut TString
}
#[inline(always)]
pub unsafe fn gco2h(o: *mut GCObject) -> *mut LuaTable {
    o as *mut LuaTable
}
#[inline(always)]
pub unsafe fn gco2cl(o: *mut GCObject) -> *mut Closure {
    o as *mut Closure
}
#[inline(always)]
pub unsafe fn gco2th(o: *mut GCObject) -> *mut lua_State {
    o as *mut lua_State
}

// ---------------------------------------------------------------------------
// lobject.cpp
// ---------------------------------------------------------------------------

pub fn luaO_log2(mut x: u32) -> i32 {
    let mut l = -1;
    while x >= 256 {
        l += 8;
        x >>= 8;
    }
    l + (32 - (x as u32).leading_zeros() as i32)
}

#[inline]
pub fn ceillog2(x: u32) -> i32 {
    luaO_log2(x.wrapping_sub(1)) + 1
}

#[inline(always)]
pub unsafe fn luai_veceq(a: *const f32, b: *const f32) -> bool {
    *a == *b && *a.add(1) == *b.add(1) && f32::from_bits(*(a.add(2) as *const u32)) == f32::from_bits(*(b.add(2) as *const u32))
}

pub unsafe fn luaO_rawequalObj(t1: *const TValue, t2: *const TValue) -> bool {
    if (*t1).tt != (*t2).tt {
        return false;
    }
    match (*t1).tt {
        LUA_TNIL => true,
        LUA_TNUMBER => (*t1).value.n == (*t2).value.n,
        LUA_TINTEGER => (*t1).value.l == (*t2).value.l,
        LUA_TVECTOR => luai_veceq(vvalue(t1), vvalue(t2)),
        LUA_TBOOLEAN => (*t1).value.b == (*t2).value.b,
        LUA_TLIGHTUSERDATA => (*t1).value.p == (*t2).value.p && (*t1).extra[0] == (*t2).extra[0],
        _ => (*t1).value.gc == (*t2).value.gc,
    }
}

#[inline(always)]
pub unsafe fn luaO_rawequalKey(t1: *const TKey, t2: *const TValue) -> bool {
    let tt = (*t1).tt();
    if tt != (*t2).tt {
        return false;
    }
    match tt {
        LUA_TNIL => true,
        LUA_TNUMBER => (*t1).value.n == (*t2).value.n,
        LUA_TINTEGER => (*t1).value.l == (*t2).value.l,
        LUA_TVECTOR => {
            let a = ptr::addr_of!((*t1).value.v) as *const f32;
            luai_veceq(a, vvalue(t2))
        }
        LUA_TBOOLEAN => (*t1).value.b == (*t2).value.b,
        LUA_TLIGHTUSERDATA => (*t1).value.p == (*t2).value.p && (*t1).extra[0] == (*t2).extra[0],
        _ => (*t1).value.gc == (*t2).value.gc,
    }
}

/// `luaO_str2d`: `strtod` semantics with hex fallback and trailing space.
pub fn luaO_str2d(s: &[u8]) -> Option<f64> {
    let (v, end) = crate::lnumutils::strtod(s);
    if end == 0 {
        return None;
    }
    let mut v = v;
    let mut end = end;
    if end < s.len() && (s[end] == b'x' || s[end] == b'X') {
        let (u, e) = crate::lnumutils::strtoul(s, 16);
        v = u as f64;
        end = e;
    }
    while end < s.len() && crate::lnumutils::isspace(s[end]) {
        end += 1;
    }
    if end != s.len() {
        return None;
    }
    Some(v)
}

/// `luaO_str2l`.
pub fn luaO_str2l(s: &[u8], base: u32) -> Option<i64> {
    let (mut v, mut end);
    if base == 10 {
        let (x, e) = crate::lnumutils::strtoll(s, 10);
        v = x;
        end = e;
        if end == 0 {
            return None;
        }
        if end < s.len() && (s[end] == b'x' || s[end] == b'X') {
            let (u, e) = crate::lnumutils::strtoull(s, 16);
            v = u as i64;
            end = e;
        }
    } else {
        let (u, e) = crate::lnumutils::strtoull(s, base);
        v = u as i64;
        end = e;
        if end == 0 {
            return None;
        }
    }
    while end < s.len() && crate::lnumutils::isspace(s[end]) {
        end += 1;
    }
    if end != s.len() {
        return None;
    }
    Some(v)
}

/// `luaO_pushfstring`: pushes an already formatted message (truncated to
/// `LUA_BUFFERSIZE - 1` like the C++ `vsnprintf` buffer).
pub unsafe fn luaO_pushfstring(L: *mut lua_State, msg: &str) -> *mut TString {
    let bytes = msg.as_bytes();
    let n = bytes.len().min(LUA_BUFFERSIZE - 1);
    let ts = crate::lstring::luaS_newlstr(L, &bytes[..n]);
    setsvalue((*L).top, ts);
    crate::ldo::incr_top(L);
    ts
}

/// `luaO_chunkid`: display form of a chunk name for messages.
pub fn luaO_chunkid(source: &[u8], buflen: usize) -> Vec<u8> {
    let srclen = source.len();
    match source.first() {
        Some(b'=') => {
            if srclen <= buflen {
                return source[1..].to_vec();
            }
            source[1..buflen].to_vec()
        }
        Some(b'@') => {
            if srclen <= buflen {
                return source[1..].to_vec();
            }
            let mut out = b"...".to_vec();
            out.extend_from_slice(&source[srclen - (buflen - 4)..]);
            out.truncate(buflen - 1);
            out
        }
        _ => {
            let mut len = source.iter().position(|&c| c == 10 || c == 13).unwrap_or(srclen);
            let room = buflen - b"[string \"...\"]".len() - 1;
            if len > room {
                len = room;
            }
            let mut out = b"[string \"".to_vec();
            if len < srclen {
                out.extend_from_slice(&source[..len]);
                out.extend_from_slice(b"...");
            } else {
                out.extend_from_slice(source);
            }
            out.extend_from_slice(b"\"]");
            out
        }
    }
}
