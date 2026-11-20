//! # luaurs
//!
//! Embed [Luau](https://luau.org) in Rust applications: a pure-Rust Luau
//! runtime, compiler and x64 JIT, with a small safe API on top.
//!
//! ```no_run
//! use luaurs::{Luau, Value};
//!
//! let lua = Luau::new();
//! lua.load("print('hello from Luau')").exec().unwrap();
//!
//! let sum: f64 = lua.load("return 1 + 2").eval().unwrap();
//! assert_eq!(sum, 3.0);
//!
//! let greet = lua.create_function(|_, args| {
//!     let name = args.first().and_then(|v| v.as_str()).unwrap_or("world").to_string();
//!     Ok(vec![Value::from(format!("hello, {name}"))])
//! });
//! lua.globals().set("greet", greet).unwrap();
//! let s: String = lua.load("return greet('Luau')").eval().unwrap();
//! assert_eq!(s, "hello, Luau");
//! ```
//!
//! The lower layers are re-exported for advanced use: [`luau_rt`] (the VM
//! with its C-style API) and [`luau_compiler`].

#![allow(non_snake_case, clippy::missing_safety_doc)]

pub use luau_compiler;
pub use luau_rt;

use luau_compiler::CompileOptions;
use luau_rt::lapi::*;
use luau_rt::laux::*;
use luau_rt::ldo::lua_resume;
use luau_rt::linit::{
    luaL_newstate, luaL_openextlibs, luaL_openlibs, luaL_sandbox, luaL_sandboxthread,
};
use luau_rt::lobject::*;
use luau_rt::lstate::lua_close;
use luau_rt::ltasklib as task;
use luau_rt::lvmload::luau_load;
use std::any::{Any, TypeId};
use std::cell::{Cell, Ref as CellRef, RefCell, RefMut as CellRefMut};
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

// -- errors ----------------------------------------------------------------------

/// A Luau error: compile errors, runtime errors (with the message the script
/// raised) and conversion errors.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub message: String,
    /// Luau stack trace for runtime errors, when one is available.
    pub traceback: Option<String>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            traceback: None,
        }
    }
}

impl Error {
    fn runtime(message: String, traceback: String) -> Error {
        Error {
            message,
            traceback: Some(traceback),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(traceback) = &self.traceback {
            if !traceback.is_empty() {
                write!(f, "\n{traceback}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// -- options -----------------------------------------------------------------------

/// How a [`Luau`] state is set up.
#[derive(Debug, Clone)]
pub struct Options {
    /// Compile loaded code to native x64 code (only where supported; ignored
    /// elsewhere). Default: on.
    pub codegen: bool,
    /// Compiler optimization level, 0..=2. Default: 1.
    pub optimization_level: u8,
    /// Debug info level, 0..=2. Default: 1.
    pub debug_level: u8,
    /// Freeze the standard libraries and globals (scripts get a private
    /// global table that reads through to them), like the `luau` CLI.
    /// Default: off.
    pub sandbox: bool,
    /// Install `task`, `Signal`, `Regex` and the debug extensions. Default: on.
    pub task_library: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            codegen: true,
            optimization_level: 1,
            debug_level: 1,
            sandbox: false,
            task_library: true,
        }
    }
}

// -- the state ---------------------------------------------------------------------

struct Inner {
    L: *mut lua_State,
    opts: Options,
    /// set while a Rust callback runs (no re-entrant close)
    depth: Cell<u32>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe { lua_close(self.L) }
    }
}

/// A Luau state. Cheap to clone (all clones share the state).
#[derive(Clone)]
pub struct Luau {
    inner: Rc<Inner>,
}

impl Default for Luau {
    fn default() -> Self {
        Luau::new()
    }
}

impl Luau {
    /// A state with the standard libraries (and `task`/`Signal`).
    pub fn new() -> Luau {
        Luau::with_options(Options::default())
    }

    pub fn with_options(opts: Options) -> Luau {
        unsafe {
            let L = luaL_newstate();
            luaL_openlibs(L);
            if opts.task_library {
                luaL_openextlibs(L);
            }
            if opts.sandbox {
                luaL_sandbox(L);
                luaL_sandboxthread(L);
            }
            Luau {
                inner: Rc::new(Inner {
                    L,
                    opts,
                    depth: Cell::new(0),
                }),
            }
        }
    }

    /// The raw `lua_State` (for the [`luau_rt`] C-style API).
    pub fn state(&self) -> *mut lua_State {
        self.inner.L
    }

    fn L(&self) -> *mut lua_State {
        self.inner.L
    }

    fn compile_options(&self) -> CompileOptions {
        CompileOptions {
            optimization_level: self.inner.opts.optimization_level,
            debug_level: self.inner.opts.debug_level,
            ..Default::default()
        }
    }

    /// Compiles source to Luau bytecode.
    pub fn compile(&self, source: impl AsRef<[u8]>) -> Result<Vec<u8>> {
        let bc = luau_compiler::compile_source(source.as_ref(), self.compile_options());
        // a compile error is encoded as version byte 0 followed by the message
        if bc.first() == Some(&0) {
            return Err(Error::new(String::from_utf8_lossy(&bc[1..]).into_owned()));
        }
        Ok(bc)
    }

    /// Prepares a chunk of source code (or bytecode, see [`Chunk::bytecode`]).
    pub fn load(&self, source: impl AsRef<[u8]>) -> Chunk<'_> {
        Chunk {
            lua: self,
            source: source.as_ref().to_vec(),
            name: "=chunk".to_string(),
            is_bytecode: false,
        }
    }

    /// The global table.
    pub fn globals(&self) -> Table {
        unsafe {
            lua_pushvalue(self.L(), LUA_GLOBALSINDEX);
            Table(Ref::pop(self))
        }
    }

    pub fn set_global(&self, name: &str, value: impl IntoValue) -> Result<()> {
        self.globals().set(name, value)
    }

    pub fn get_global<T: FromValue>(&self, name: &str) -> Result<T> {
        self.globals().get(name)
    }

    pub fn create_table(&self) -> Table {
        unsafe {
            lua_createtable(self.L(), 0, 0);
            Table(Ref::pop(self))
        }
    }

    /// Exposes a Rust value as full userdata owned by the Luau garbage collector.
    /// The returned handle can borrow the value and attach a Luau metatable.
    pub fn create_userdata<T: 'static>(&self, value: T) -> UserData<T> {
        unsafe {
            let L = self.L();
            let slot = lua_newuserdatadtor(L, std::mem::size_of::<UserDataCell>(), drop_userdata)
                as *mut UserDataCell;
            slot.write(UserDataCell {
                magic: USERDATA_MAGIC,
                type_id: TypeId::of::<T>(),
                value: RefCell::new(Box::new(value)),
            });
            lua_createtable(L, 0, 1);
            lua_pushstring(L, "RustUserData");
            lua_setfield(L, -2, "__type");
            lua_setmetatable(L, -2);
            UserData {
                reference: Ref::pop(self),
                marker: PhantomData,
            }
        }
    }

    /// Converts supported Rust values, including vectors and string-keyed maps,
    /// into Luau values. Tables are allocated in this state.
    pub fn to_value<T: IntoValue>(&self, value: T) -> Result<Value> {
        value.into_value(self)
    }

    /// A Luau function implemented by a Rust closure. Errors it returns are
    /// raised as Luau errors.
    pub fn create_function<F>(&self, f: F) -> Function
    where
        F: Fn(&Luau, Vec<Value>) -> Result<Vec<Value>> + 'static,
    {
        unsafe {
            let L = self.L();
            let cb: Box<Callback> = Box::new(Callback {
                f: Box::new(f),
                lua: Rc::downgrade(&self.inner),
            });
            let slot = lua_newuserdatadtor(L, std::mem::size_of::<*mut Callback>(), drop_callback)
                as *mut *mut Callback;
            *slot = Box::into_raw(cb);
            lua_pushcclosurek(L, trampoline, Some("rust function"), 1, None);
            Function(Ref::pop(self))
        }
    }

    /// Runs work scheduled with `task.defer`, `task.delay`, `task.wait` and
    /// `Signal:Wait` until nothing is left (call after `exec` when scripts use
    /// the task library).
    pub fn run_scheduler(&self) -> Result<()> {
        unsafe {
            let L = self.L();
            let T = lua_newthread(L);
            let r = task::run_scheduler(L, T);
            lua_pop(L, 1);
            match r {
                task::MainOutcome::Done => Ok(()),
                task::MainOutcome::Error => Err(runtime_error(T)),
                _ => Err(Error::new("scheduler error")),
            }
        }
    }

    /// Runs a full garbage collection.
    pub fn gc_collect(&self) {
        unsafe {
            lua_gc(self.L(), LUA_GCCOLLECT, 0);
        }
    }

    /// Memory in use by the state, in kilobytes.
    pub fn used_memory_kb(&self) -> i32 {
        unsafe { lua_gc(self.L(), LUA_GCCOUNT, 0) }
    }

    /// Converts the value at `idx` of the stack of thread `L` (this state's).
    unsafe fn value_at(&self, L: *mut lua_State, idx: i32) -> Value {
        match lua_type(L, idx) {
            LUA_TNIL | LUA_TNONE => Value::Nil,
            LUA_TBOOLEAN => Value::Boolean(lua_toboolean(L, idx) != 0),
            LUA_TNUMBER => Value::Number(lua_tonumberx(L, idx, None)),
            LUA_TINTEGER => Value::Integer(lua_tointeger64(L, idx, None)),
            LUA_TVECTOR => Value::Vector(lua_tovector(L, idx).unwrap_or([0.0; 3])),
            LUA_TSTRING => Value::String(LuauString(lua_tolstring(L, idx).unwrap_or(b"").to_vec())),
            LUA_TTABLE => Value::Table(Table(Ref::at(self, L, idx))),
            LUA_TFUNCTION => Value::Function(Function(Ref::at(self, L, idx))),
            _ => Value::Other(Ref::at(self, L, idx)),
        }
    }

    /// Pushes `v` onto the stack of thread `L` (this state's).
    unsafe fn push(&self, L: *mut lua_State, v: &Value) {
        match v {
            Value::Nil => lua_pushnil(L),
            Value::Boolean(b) => lua_pushboolean(L, *b as i32),
            Value::Number(n) => lua_pushnumber(L, *n),
            Value::Integer(i) => lua_pushinteger64(L, *i),
            Value::Vector(v) => lua_pushvector(L, v[0], v[1], v[2]),
            Value::String(s) => lua_pushlstring(L, &s.0),
            Value::Table(Table(r)) | Value::Function(Function(r)) | Value::Other(r) => {
                lua_rawgeti(L, LUA_REGISTRYINDEX, r.id);
            }
        }
    }

    /// Calls the function below `nargs` arguments on a fresh thread (so the
    /// task library can suspend it); returns its results.
    unsafe fn call_on_thread(&self, nargs: i32) -> Result<Vec<Value>> {
        let L = self.L();
        let T = lua_newthread(L);
        if self.inner.opts.sandbox {
            luaL_sandboxthread(T);
        }
        lua_insert(L, -(nargs + 2));
        lua_xmove(L, T, nargs + 1);
        let status = lua_resume(T, std::ptr::null_mut(), nargs);
        let result = if status == LUA_OK {
            let n = lua_gettop(T);
            let vals = (1..=n).map(|i| self.value_at(T, i)).collect();
            lua_settop(T, 0);
            Ok(vals)
        } else if status == LUA_YIELD {
            // suspended in task.wait etc.: keep running scheduled work
            match task::run_scheduler(L, T) {
                task::MainOutcome::Done => Ok(Vec::new()),
                task::MainOutcome::Error => Err(runtime_error(T)),
                task::MainOutcome::Unexpected => Err(Error::new("thread yielded unexpectedly")),
            }
        } else {
            Err(runtime_error(T))
        };
        lua_pop(L, 1);
        // the runtime buffers `print`; keep its output in order with the host's
        luau_rt::lbaselib::flush_stdout();
        result
    }
}

unsafe fn error_message(T: *mut lua_State) -> String {
    match lua_tolstring(T, -1) {
        Some(s) => String::from_utf8_lossy(s).into_owned(),
        None => "(error object is not a string)".to_string(),
    }
}

unsafe fn runtime_error(T: *mut lua_State) -> Error {
    let message = error_message(T);
    luaL_traceback(T, T, None, 0);
    let traceback = lua_tolstring(T, -1)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    lua_pop(T, 1);
    Error::runtime(message, traceback)
}

// -- chunks ------------------------------------------------------------------------

/// Code ready to run; see [`Luau::load`].
pub struct Chunk<'a> {
    lua: &'a Luau,
    source: Vec<u8>,
    name: String,
    is_bytecode: bool,
}

impl<'a> Chunk<'a> {
    /// The chunk name used in error messages and tracebacks.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        let n = name.into();
        self.name = if n.starts_with('=') || n.starts_with('@') {
            n
        } else {
            format!("={n}")
        };
        self
    }

    /// The source is precompiled bytecode (from [`Luau::compile`]).
    pub fn bytecode(mut self) -> Self {
        self.is_bytecode = true;
        self
    }

    /// Compiles and loads the chunk as a function without running it.
    pub fn into_function(self) -> Result<Function> {
        unsafe {
            self.push_function()?;
            Ok(Function(Ref::pop(self.lua)))
        }
    }

    unsafe fn push_function(&self) -> Result<()> {
        let L = self.lua.L();
        let bc = if self.is_bytecode {
            self.source.clone()
        } else {
            self.lua.compile(&self.source)?
        };
        if luau_load(L, self.name.as_bytes(), &bc, 0) != 0 {
            let msg = error_message(L);
            lua_pop(L, 1);
            return Err(Error::new(msg));
        }
        #[cfg(all(target_arch = "x86_64", windows))]
        if self.lua.inner.opts.codegen && luau_rt::codegen::supported() {
            luau_rt::codegen::compile(L, -1);
        }
        Ok(())
    }

    /// Runs the chunk (and any work it schedules with the task library).
    pub fn exec(self) -> Result<()> {
        self.call(()).map(|_| ())
    }

    /// Runs the chunk and converts its first result.
    pub fn eval<T: FromValue>(self) -> Result<T> {
        let lua = self.lua.clone();
        let vals = self.call(())?;
        T::from_value(vals.into_iter().next().unwrap_or(Value::Nil), &lua)
    }

    /// Runs the chunk with arguments (its `...`); returns all results.
    pub fn call(self, args: impl IntoArgs) -> Result<Vec<Value>> {
        unsafe {
            self.push_function()?;
            let args = args.into_args();
            for a in &args {
                self.lua.push(self.lua.L(), a);
            }
            let r = self.lua.call_on_thread(args.len() as i32);
            if r.is_ok() {
                // deferred/delayed work the chunk scheduled
                self.lua.run_scheduler()?;
            }
            r
        }
    }
}

// -- references --------------------------------------------------------------------

/// A registry reference keeping a Luau object alive.
pub struct Ref {
    lua: Luau,
    id: i32,
}

impl Ref {
    unsafe fn pop(lua: &Luau) -> Ref {
        let L = lua.L();
        let id = lua_ref(L, -1);
        lua_pop(L, 1);
        Ref {
            lua: lua.clone(),
            id,
        }
    }

    unsafe fn at(lua: &Luau, L: *mut lua_State, idx: i32) -> Ref {
        let id = lua_ref(L, idx);
        Ref {
            lua: lua.clone(),
            id,
        }
    }

    unsafe fn push(&self) {
        lua_rawgeti(self.lua.L(), LUA_REGISTRYINDEX, self.id);
    }
}

impl Clone for Ref {
    fn clone(&self) -> Ref {
        unsafe {
            self.push();
            Ref::pop(&self.lua)
        }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        unsafe {
            lua_unref(self.lua.L(), self.id);
        }
    }
}

impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ref({})", self.id)
    }
}

impl PartialEq for Ref {
    fn eq(&self, other: &Ref) -> bool {
        unsafe {
            let L = self.lua.L();
            self.push();
            other.push();
            let eq = lua_rawequal(L, -1, -2) != 0;
            lua_pop(L, 2);
            eq
        }
    }
}

// -- values ------------------------------------------------------------------------

const USERDATA_MAGIC: u64 = 0x4c55_4155_5253_5544;

struct UserDataCell {
    magic: u64,
    type_id: TypeId,
    value: RefCell<Box<dyn Any>>,
}

unsafe fn drop_userdata(_L: *mut lua_State, p: *mut u8) {
    // A Rust destructor must not unwind through Luau's C-style GC frames.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::ptr::drop_in_place(p as *mut UserDataCell);
    }));
}

/// A Luau string (arbitrary bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuauString(pub Vec<u8>);

impl LuauString {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

/// A Luau value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    Vector([f32; 3]),
    String(LuauString),
    Table(Table),
    Function(Function),
    /// threads, userdata, buffers, ...
    Other(Ref),
}

impl Value {
    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => s.to_str(),
            _ => None,
        }
    }
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
    /// Luau truthiness (everything but nil and false).
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Boolean(false))
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Boolean(_) => "boolean",
            Value::Number(_) => "number",
            Value::Integer(_) => "integer",
            Value::Vector(_) => "vector",
            Value::String(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
            Value::Other(_) => "userdata",
        }
    }
}

macro_rules! from_num {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Value { Value::Number(v as f64) }
        }
    )*};
}
from_num!(f64, f32, i32, u32, i16, u16, i8, u8, usize, isize);

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Number(v as f64)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Boolean(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(LuauString(v.as_bytes().to_vec()))
    }
}
impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(LuauString(v.into_bytes()))
    }
}
impl From<Table> for Value {
    fn from(v: Table) -> Value {
        Value::Table(v)
    }
}
impl From<Function> for Value {
    fn from(v: Function) -> Value {
        Value::Function(v)
    }
}
impl From<()> for Value {
    fn from(_: ()) -> Value {
        Value::Nil
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Nil, Into::into)
    }
}

/// Conversion from a Luau value.
pub trait FromValue: Sized {
    fn from_value(v: Value, lua: &Luau) -> Result<Self>;
}

fn conv_err<T>(v: &Value, want: &str) -> Result<T> {
    Err(Error::new(format!(
        "cannot convert {} to {}",
        v.type_name(),
        want
    )))
}

impl FromValue for Value {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        Ok(v)
    }
}
impl FromValue for f64 {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        v.as_number().map_or_else(|| conv_err(&v, "number"), Ok)
    }
}
impl FromValue for f32 {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        f64::from_value(v, lua).map(|n| n as f32)
    }
}
impl FromValue for i64 {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        match v {
            Value::Integer(i) => Ok(i),
            Value::Number(n) if n.fract() == 0.0 => Ok(n as i64),
            _ => conv_err(&v, "integer"),
        }
    }
}
impl FromValue for i32 {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        i64::from_value(v, lua).map(|n| n as i32)
    }
}
impl FromValue for usize {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        i64::from_value(v, lua).map(|n| n as usize)
    }
}
impl FromValue for bool {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        Ok(v.truthy())
    }
}
impl FromValue for String {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        match &v {
            Value::String(s) => Ok(String::from_utf8_lossy(&s.0).into_owned()),
            Value::Number(n) => Ok(n.to_string()),
            _ => conv_err(&v, "string"),
        }
    }
}
impl FromValue for Table {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        match v {
            Value::Table(t) => Ok(t),
            _ => conv_err(&v, "table"),
        }
    }
}
impl FromValue for Function {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        match v {
            Value::Function(f) => Ok(f),
            _ => conv_err(&v, "function"),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        if v.is_nil() {
            Ok(None)
        } else {
            T::from_value(v, lua).map(Some)
        }
    }
}

/// A typed, GC-rooted handle to Rust-owned Luau userdata.
pub struct UserData<T: 'static> {
    reference: Ref,
    marker: PhantomData<fn() -> T>,
}

impl<T: 'static> Clone for UserData<T> {
    fn clone(&self) -> Self {
        Self {
            reference: self.reference.clone(),
            marker: PhantomData,
        }
    }
}

impl<T: 'static> fmt::Debug for UserData<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserData")
            .field("type_name", &std::any::type_name::<T>())
            .finish()
    }
}

impl<T: 'static> UserData<T> {
    fn cell(&self) -> Result<&UserDataCell> {
        unsafe {
            self.reference.push();
            let L = self.reference.lua.L();
            let ptr = lua_touserdata(L, -1) as *mut UserDataCell;
            let valid = !ptr.is_null()
                && lua_objlen(L, -1) >= std::mem::size_of::<UserDataCell>() as i32
                && (*ptr).magic == USERDATA_MAGIC
                && (*ptr).type_id == TypeId::of::<T>();
            lua_pop(L, 1);
            if valid {
                Ok(&*ptr)
            } else {
                Err(Error::new("userdata has a different Rust type"))
            }
        }
    }

    /// Borrows the Rust value immutably.
    pub fn borrow(&self) -> Result<CellRef<'_, T>> {
        let cell = self.cell()?;
        CellRef::filter_map(
            cell.value
                .try_borrow()
                .map_err(|_| Error::new("userdata is already mutably borrowed"))?,
            |v| v.downcast_ref::<T>(),
        )
        .map_err(|_| Error::new("userdata has a different Rust type"))
    }

    /// Borrows the Rust value mutably.
    pub fn borrow_mut(&self) -> Result<CellRefMut<'_, T>> {
        let cell = self.cell()?;
        CellRefMut::filter_map(
            cell.value
                .try_borrow_mut()
                .map_err(|_| Error::new("userdata is already borrowed"))?,
            |v| v.downcast_mut::<T>(),
        )
        .map_err(|_| Error::new("userdata has a different Rust type"))
    }

    /// Replaces the object's metatable. The table is rooted for as long as it is used.
    pub fn set_metatable(&self, metatable: &Table) -> Result<()> {
        unsafe {
            let L = self.reference.lua.L();
            self.reference.push();
            metatable.0.push();
            lua_setmetatable(L, -2);
            lua_pop(L, 1);
        }
        Ok(())
    }
}

impl<T: 'static> IntoValue for UserData<T> {
    fn into_value(self, _: &Luau) -> Result<Value> {
        Ok(Value::Other(self.reference))
    }
}

impl<T: 'static> FromValue for UserData<T> {
    fn from_value(v: Value, _: &Luau) -> Result<Self> {
        match v {
            Value::Other(reference) => {
                let value = UserData {
                    reference,
                    marker: PhantomData,
                };
                value.cell()?;
                Ok(value)
            }
            _ => conv_err(&v, "userdata"),
        }
    }
}

/// State-aware conversion into Luau values.
pub trait IntoValue {
    fn into_value(self, lua: &Luau) -> Result<Value>;
}

macro_rules! into_value_via_from {
    ($($t:ty),* $(,)?) => {$ (
        impl IntoValue for $t {
            fn into_value(self, _: &Luau) -> Result<Value> { Ok(self.into()) }
        }
    )*};
}
into_value_via_from!(
    Value,
    bool,
    f64,
    f32,
    i64,
    i32,
    u32,
    i16,
    u16,
    i8,
    u8,
    usize,
    isize,
    String,
    Table,
    Function,
    ()
);

impl IntoValue for LuauString {
    fn into_value(self, _: &Luau) -> Result<Value> {
        Ok(Value::String(self))
    }
}

impl IntoValue for &str {
    fn into_value(self, _: &Luau) -> Result<Value> {
        Ok(Value::from(self))
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self, lua: &Luau) -> Result<Value> {
        match self {
            Some(value) => value.into_value(lua),
            None => Ok(Value::Nil),
        }
    }
}

impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value(self, lua: &Luau) -> Result<Value> {
        let table = lua.create_table();
        for (i, item) in self.into_iter().enumerate() {
            table.raw_set((i + 1) as f64, item.into_value(lua)?)?;
        }
        Ok(Value::Table(table))
    }
}

impl<T: IntoValue> IntoValue for HashMap<String, T> {
    fn into_value(self, lua: &Luau) -> Result<Value> {
        let table = lua.create_table();
        for (key, item) in self {
            table.raw_set(key, item.into_value(lua)?)?;
        }
        Ok(Value::Table(table))
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        let table = Table::from_value(v, lua)?;
        table
            .sequence()
            .into_iter()
            .map(|item| T::from_value(item, lua))
            .collect()
    }
}

impl<T: FromValue> FromValue for HashMap<String, T> {
    fn from_value(v: Value, lua: &Luau) -> Result<Self> {
        let table = Table::from_value(v, lua)?;
        table
            .pairs()
            .into_iter()
            .map(|(k, value)| {
                let key = String::from_value(k, lua)?;
                let value = T::from_value(value, lua)?;
                Ok((key, value))
            })
            .collect()
    }
}

/// Function arguments: `()`, a single value, a tuple, or a `Vec<Value>`.
pub trait IntoArgs {
    fn into_args(self) -> Vec<Value>;
}
impl IntoArgs for () {
    fn into_args(self) -> Vec<Value> {
        Vec::new()
    }
}
impl IntoArgs for Vec<Value> {
    fn into_args(self) -> Vec<Value> {
        self
    }
}
impl<T: Into<Value>> IntoArgs for (T,) {
    fn into_args(self) -> Vec<Value> {
        vec![self.0.into()]
    }
}
impl<A: Into<Value>, B: Into<Value>> IntoArgs for (A, B) {
    fn into_args(self) -> Vec<Value> {
        vec![self.0.into(), self.1.into()]
    }
}
impl<A: Into<Value>, B: Into<Value>, C: Into<Value>> IntoArgs for (A, B, C) {
    fn into_args(self) -> Vec<Value> {
        vec![self.0.into(), self.1.into(), self.2.into()]
    }
}

// -- tables ------------------------------------------------------------------------

/// A reference to a Luau table.
#[derive(Clone, PartialEq)]
pub struct Table(Ref);

impl fmt::Debug for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Table({})", self.0.id)
    }
}

impl Table {
    /// `t[key] = value` (metamethods apply).
    pub fn set(&self, key: impl IntoValue, value: impl IntoValue) -> Result<()> {
        let lua = &self.0.lua;
        let (k, v) = (key.into_value(lua)?, value.into_value(lua)?);
        self.protected(move |lua, L| unsafe {
            lua.push(L, &k);
            lua.push(L, &v);
            lua_settable(L, -3);
            0
        })
        .map(|_| ())
    }

    /// `t[key]` (metamethods apply).
    pub fn get<T: FromValue>(&self, key: impl Into<Value>) -> Result<T> {
        let k = key.into();
        let lua = self.0.lua.clone();
        let vals = self.protected(move |lua, L| unsafe {
            lua.push(L, &k);
            lua_gettable(L, -2);
            1
        })?;
        T::from_value(vals.into_iter().next().unwrap_or(Value::Nil), &lua)
    }

    /// Gets a value, converting `default` only if the field is nil.
    pub fn get_or<T: FromValue>(&self, key: impl IntoValue, default: impl IntoValue) -> Result<T> {
        let lua = self.0.lua.clone();
        let key = key.into_value(&lua)?;
        let vals = self.protected(move |lua, L| unsafe {
            lua.push(L, &key);
            lua_gettable(L, -2);
            1
        })?;
        let value = vals.into_iter().next().unwrap_or(Value::Nil);
        T::from_value(
            if value.is_nil() {
                default.into_value(&lua)?
            } else {
                value
            },
            &lua,
        )
    }

    /// Typed raw key/value iteration. Conversion errors are returned instead of discarded.
    pub fn pairs_typed<K: FromValue, V: FromValue>(&self) -> Result<Vec<(K, V)>> {
        let lua = self.0.lua.clone();
        self.pairs()
            .into_iter()
            .map(|(key, value)| Ok((K::from_value(key, &lua)?, V::from_value(value, &lua)?)))
            .collect()
    }

    /// Typed iteration over the array part `t[1..n]`.
    pub fn sequence_typed<T: FromValue>(&self) -> Result<Vec<T>> {
        let lua = self.0.lua.clone();
        self.sequence()
            .into_iter()
            .map(|value| T::from_value(value, &lua))
            .collect()
    }

    /// `rawget(t, key)`.
    pub fn raw_get<T: FromValue>(&self, key: impl IntoValue) -> Result<T> {
        let lua = self.0.lua.clone();
        let key = key.into_value(&lua)?;
        unsafe {
            let L = lua.L();
            self.0.push();
            lua.push(L, &key);
            lua_rawget(L, -2);
            let v = lua.value_at(L, -1);
            lua_pop(L, 2);
            T::from_value(v, &lua)
        }
    }

    /// `rawset(t, key, value)`.
    pub fn raw_set(&self, key: impl IntoValue, value: impl IntoValue) -> Result<()> {
        let lua = &self.0.lua;
        let k = key.into_value(lua)?;
        let v = value.into_value(lua)?;
        unsafe {
            let L = lua.L();
            self.0.push();
            if lua_getreadonly(L, -1) != 0 {
                lua_pop(L, 1);
                return Err(Error::new("attempt to modify a readonly table"));
            }
            if k.is_nil() || matches!(k, Value::Number(n) if n.is_nan()) {
                lua_pop(L, 1);
                return Err(Error::new("invalid table key"));
            }
            lua.push(L, &k);
            lua.push(L, &v);
            lua_rawset(L, -3);
            lua_pop(L, 1);
        }
        Ok(())
    }

    /// The length (`#t`, without metamethods).
    pub fn len(&self) -> usize {
        unsafe {
            self.0.push();
            let n = lua_objlen(self.0.lua.L(), -1);
            lua_pop(self.0.lua.L(), 1);
            n as usize
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pairs().is_empty()
    }

    /// All key/value pairs (raw iteration).
    pub fn pairs(&self) -> Vec<(Value, Value)> {
        let lua = &self.0.lua;
        let mut out = Vec::new();
        unsafe {
            let L = lua.L();
            self.0.push();
            let t = lua_gettop(L);
            let mut it = 0;
            loop {
                it = lua_rawiter(L, t, it);
                if it < 0 {
                    break;
                }
                out.push((lua.value_at(L, -2), lua.value_at(L, -1)));
                lua_pop(L, 2);
            }
            lua_pop(L, 1);
        }
        out
    }

    /// The array part `t[1..n]`.
    pub fn sequence(&self) -> Vec<Value> {
        (1..=self.len())
            .map(|i| self.raw_get(i as f64).unwrap_or(Value::Nil))
            .collect()
    }

    /// Runs `f` with the table on top of the stack inside a protected call;
    /// returns the values `f` reports leaving on top.
    fn protected<F>(&self, f: F) -> Result<Vec<Value>>
    where
        F: FnOnce(&Luau, *mut lua_State) -> i32 + 'static,
    {
        let lua = self.0.lua.clone();
        let t = self.clone();
        let f = std::cell::RefCell::new(Some(f));
        let func = lua.create_function(move |lua, _| unsafe {
            let L = lua.L();
            let base = lua_gettop(L);
            t.0.push();
            let Some(f) = f.borrow_mut().take() else {
                return Err(Error::new("table operation re-entered"));
            };
            let n = f(lua, L);
            let vals = (0..n).map(|k| lua.value_at(L, -(n - k))).collect();
            lua_settop(L, base);
            Ok(vals)
        });
        func.call(())
    }
}

// -- functions ---------------------------------------------------------------------

/// A reference to a Luau function.
#[derive(Clone, PartialEq)]
pub struct Function(Ref);

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Function({})", self.0.id)
    }
}

impl Function {
    /// Calls the function; returns all results.
    pub fn call(&self, args: impl IntoArgs) -> Result<Vec<Value>> {
        let lua = self.0.lua.clone();
        unsafe {
            self.0.push();
            let args = args.into_args();
            for a in &args {
                lua.push(lua.L(), a);
            }
            lua.call_on_thread(args.len() as i32)
        }
    }

    /// Calls the function and converts its first result.
    pub fn call_get<T: FromValue>(&self, args: impl IntoArgs) -> Result<T> {
        let lua = self.0.lua.clone();
        let vals = self.call(args)?;
        T::from_value(vals.into_iter().next().unwrap_or(Value::Nil), &lua)
    }
}

// -- Rust callbacks ----------------------------------------------------------------

type CallbackFn = dyn Fn(&Luau, Vec<Value>) -> Result<Vec<Value>>;

struct Callback {
    f: Box<CallbackFn>,
    lua: Weak<Inner>,
}

unsafe fn drop_callback(_L: *mut lua_State, p: *mut u8) {
    let slot = p as *mut *mut Callback;
    if !(*slot).is_null() {
        drop(Box::from_raw(*slot));
        *slot = std::ptr::null_mut();
    }
}

/// The C function behind [`Luau::create_function`].
unsafe fn trampoline(L: *mut lua_State) -> i32 {
    let slot = lua_touserdata(L, lua_upvalueindex(1)) as *mut *mut Callback;
    let outcome: std::result::Result<Vec<Value>, String> = (|| {
        let cb = &**slot;
        let Some(inner) = cb.lua.upgrade() else {
            return Err("Luau state is gone".to_string());
        };
        let lua = Luau { inner };
        lua.inner.depth.set(lua.inner.depth.get() + 1);
        let n = lua_gettop(L);
        let args: Vec<Value> = (1..=n).map(|i| lua.value_at(L, i)).collect();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (cb.f)(&lua, args)));
        lua.inner.depth.set(lua.inner.depth.get() - 1);
        let rets = match r {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e.message),
            Err(_) => return Err("rust function panicked".to_string()),
        };
        lua_settop(L, 0);
        lua_checkstack(L, rets.len() as i32 + 1);
        for v in &rets {
            lua.push(L, v);
        }
        Ok(rets)
    })();
    match outcome {
        Ok(rets) => rets.len() as i32,
        // nothing with a destructor is left on this frame when raising
        Err(msg) => luaL_error_owned(L, msg),
    }
}
