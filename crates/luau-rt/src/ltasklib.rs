//! `task` library: a scheduler for functions and threads (Roblox-style API,
//! not part of standard Luau; opened by `luaL_openextlibs`).
//!
//! `task.spawn` resumes threads immediately; `task.defer`, `task.delay`,
//! `task.wait` (and `Signal:Wait`) park threads that [`run_scheduler`]
//! resumes once the host's main thread returns or yields, until nothing is
//! left to run.

use crate::lapi::*;
use crate::laux::*;
use crate::ldebug::lua_debugtrace;
use crate::ldo::{lua_isyieldable, lua_resume, lua_yield};
use crate::lobject::*;
use crate::lstate::lua_resetthread;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::time::Instant;

/// A parked thread: resumed with the packed arguments, or (for `task.wait`)
/// with the time elapsed since `waitstart`.
struct Entry {
    thread: i32, // registry ref
    args: i32,   // registry ref to a packed argument table, or LUA_NOREF
    due: f64,
    seq: u64,
    waitstart: Option<f64>,
}

#[derive(Default)]
struct Sched {
    deferred: VecDeque<Entry>,
    timers: Vec<Entry>,
    seq: u64,
    /// threads suspended in task.wait / Signal:Wait (a yield of the main
    /// thread anywhere else is "unexpected")
    parked: HashSet<usize>,
    /// the script thread, and whether it failed after a scheduler resume
    main: usize,
    main_failed: bool,
}

thread_local! {
    static SCHED: RefCell<Sched> = RefCell::new(Sched::default());
    static START: Instant = Instant::now();
}

fn now() -> f64 {
    START.with(|s| s.elapsed().as_secs_f64())
}

// -- helpers ------------------------------------------------------------------------

/// Packs the values at `from..=top` of `L` into a table (with `n`); returns
/// a registry ref and leaves the stack as it was.
unsafe fn pack(L: *mut lua_State, from: i32) -> i32 {
    let top = lua_gettop(L);
    let n = (top - from + 1).max(0);
    lua_createtable(L, n, 1);
    for i in 0..n {
        lua_pushvalue(L, from + i);
        lua_rawseti(L, -2, i + 1);
    }
    lua_pushinteger(L, n);
    lua_setfield(L, -2, "n");
    let r = lua_ref(L, -1);
    lua_pop(L, 1);
    r
}

/// Pushes the packed values of `r` onto `co` (any thread can read the
/// registry); returns how many.
unsafe fn unpack_onto(co: *mut lua_State, r: i32) -> i32 {
    lua_rawgeti(co, LUA_REGISTRYINDEX, r);
    lua_getfield(co, -1, "n");
    let n = lua_tointeger(co, -1);
    lua_pop(co, 1);
    lua_checkstack(co, n + 1);
    for i in 1..=n {
        lua_rawgeti(co, -i, i);
    }
    lua_remove(co, -(n + 1));
    n
}

/// Index 1 of `L` as a thread: a function is wrapped in a new coroutine,
/// which replaces it at index 1.
unsafe fn arg_thread(L: *mut lua_State, what: &str) -> *mut lua_State {
    match lua_type(L, 1) {
        LUA_TFUNCTION => {
            let co = lua_newthread(L);
            lua_pushvalue(L, 1);
            lua_xmove(L, co, 1);
            lua_replace(L, 1);
            co
        }
        LUA_TTHREAD => lua_tothread(L, 1),
        _ => {
            let t = luaL_typename(L, 1);
            luaL_error_owned(L, format!("invalid argument #1 to '{}' (function or thread expected, got {})", what, t))
        }
    }
}

/// Prints an error raised in a scheduled thread (the script goes on).
unsafe fn report(co: *mut lua_State) {
    let msg = lua_tostring(co, -1).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_else(|| "(error object is not a string)".into());
    crate::lbaselib::flush_stdout();
    eprint!("{}\nstacktrace:\n{}", msg, String::from_utf8_lossy(&lua_debugtrace(co)));
}

/// Resumes suspended `co` with the `nargs` values on top of its stack.
/// Errors are reported (or remembered, for the script thread); values a
/// yield passes back are dropped.
pub(crate) unsafe fn resume(from: *mut lua_State, co: *mut lua_State, nargs: i32) {
    SCHED.with(|s| s.borrow_mut().parked.remove(&(co as usize)));
    let status = lua_resume(co, from, nargs);
    if status == LUA_OK || status == LUA_YIELD {
        let n = lua_gettop(co);
        if n > 0 && status == LUA_YIELD {
            lua_pop(co, n);
        }
        return;
    }
    let is_main = SCHED.with(|s| s.borrow().main == co as usize);
    if is_main {
        // left on the thread for the CLI's usual error report
        SCHED.with(|s| s.borrow_mut().main_failed = true);
    } else {
        report(co);
    }
}

pub(crate) fn is_suspended(L: *mut lua_State, co: *mut lua_State) -> bool {
    unsafe { lua_costatus(L, co) == LUA_COSUS }
}

/// Parks the running thread `L` and yields it to the scheduler.
pub(crate) unsafe fn park_and_yield(L: *mut lua_State) -> i32 {
    if lua_isyieldable(L) == 0 {
        luaL_error(L, "attempt to yield across metamethod/C-call boundary");
    }
    SCHED.with(|s| s.borrow_mut().parked.insert(L as usize));
    lua_yield(L, 0)
}

unsafe fn ref_thread(L: *mut lua_State, idx: i32) -> i32 {
    lua_pushvalue(L, idx);
    let r = lua_ref(L, -1);
    lua_pop(L, 1);
    r
}

unsafe fn release(L: *mut lua_State, e: &Entry) {
    lua_unref(L, e.thread);
    if e.args != LUA_NOREF {
        lua_unref(L, e.args);
    }
}

// -- task ----------------------------------------------------------------------------

/// `task.spawn(f | thread, ...)`: runs it now; returns the thread.
unsafe fn task_spawn(L: *mut lua_State) -> i32 {
    let co = arg_thread(L, "spawn");
    if !is_suspended(L, co) {
        luaL_error(L, "cannot spawn a thread that is not suspended");
    }
    let nargs = lua_gettop(L) - 1;
    lua_checkstack(co, nargs + 1);
    lua_xmove(L, co, nargs);
    resume(L, co, nargs);
    lua_settop(L, 1);
    1
}

/// `task.defer(f | thread, ...)`: runs it when the current resumption ends.
unsafe fn task_defer(L: *mut lua_State) -> i32 {
    arg_thread(L, "defer");
    let args = pack(L, 2);
    let thread = ref_thread(L, 1);
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.seq += 1;
        let seq = s.seq;
        s.deferred.push_back(Entry { thread, args, due: 0.0, seq, waitstart: None });
    });
    lua_settop(L, 1);
    1
}

fn schedule(thread: i32, args: i32, duration: f64, waitstart: Option<f64>) {
    let d = if duration.is_nan() || duration < 0.0 { 0.0 } else { duration };
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.seq += 1;
        let seq = s.seq;
        s.timers.push(Entry { thread, args, due: now() + d, seq, waitstart });
    });
}

/// `task.delay(seconds, f | thread, ...)`.
unsafe fn task_delay(L: *mut lua_State) -> i32 {
    let d = luaL_checknumber(L, 1);
    lua_remove(L, 1);
    if lua_gettop(L) == 0 {
        lua_pushnil(L);
    }
    arg_thread(L, "delay");
    let args = pack(L, 2);
    let thread = ref_thread(L, 1);
    schedule(thread, args, d, None);
    lua_settop(L, 1);
    1
}

/// `task.wait(seconds = 0)`: yields; returns the time that passed.
unsafe fn task_wait(L: *mut lua_State) -> i32 {
    let d = luaL_optnumber(L, 1, 0.0);
    lua_pushthread(L);
    let thread = lua_ref(L, -1);
    lua_pop(L, 1);
    schedule(thread, LUA_NOREF, d, Some(now()));
    lua_settop(L, 0);
    park_and_yield(L)
}

/// `task.cancel(thread)`: drops its schedule and closes it.
unsafe fn task_cancel(L: *mut lua_State) -> i32 {
    luaL_checktype(L, 1, LUA_TTHREAD);
    let co = lua_tothread(L, 1);
    let st = lua_costatus(L, co);
    if st != LUA_COSUS && st != LUA_COFIN && st != LUA_COERR {
        luaL_error(L, "cannot cancel a thread that is running or resuming another");
    }
    // drop every schedule of this thread
    let mut gone: Vec<Entry> = Vec::new();
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.parked.remove(&(co as usize));
        let (keep, drop): (VecDeque<Entry>, VecDeque<Entry>) = s.deferred.drain(..).partition(|e| !same_thread(L, e.thread, co));
        s.deferred = keep;
        gone.extend(drop);
        let (keep, drop): (Vec<Entry>, Vec<Entry>) = s.timers.drain(..).partition(|e| !same_thread(L, e.thread, co));
        s.timers = keep;
        gone.extend(drop);
    });
    for e in &gone {
        release(L, e);
    }
    lua_resetthread(co);
    0
}

fn same_thread(L: *mut lua_State, r: i32, co: *mut lua_State) -> bool {
    unsafe {
        lua_rawgeti(L, LUA_REGISTRYINDEX, r);
        let t = lua_tothread(L, -1);
        lua_pop(L, 1);
        t == co
    }
}

/// No parallel execution phases in the CLI: kept for compatibility.
unsafe fn task_noop(_L: *mut lua_State) -> i32 {
    0
}

pub(crate) unsafe fn set_fn(L: *mut lua_State, name: &str, f: lua_CFunction) {
    lua_pushcclosurek(L, f, Some(name), 0, None);
    lua_setfield(L, -2, name);
}

/// Opens the `task` library.
pub unsafe fn luaopen_task(L: *mut lua_State) -> i32 {
    // task
    lua_newtable(L);
    set_fn(L, "spawn", task_spawn);
    set_fn(L, "defer", task_defer);
    set_fn(L, "delay", task_delay);
    set_fn(L, "wait", task_wait);
    set_fn(L, "cancel", task_cancel);
    set_fn(L, "desynchronize", task_noop);
    set_fn(L, "synchronize", task_noop);
    lua_setglobal(L, "task");
    0
}

/// Outcome of running the scheduler for a script thread.
pub enum MainOutcome {
    /// finished (or was never resumed by the scheduler)
    Done,
    /// failed after the scheduler resumed it (the error is on top of its stack)
    Error,
    /// yielded outside the task scheduler
    Unexpected,
}

/// Runs scheduled work until none is left; `L` is the script's thread.
pub unsafe fn run_scheduler(GL: *mut lua_State, L: *mut lua_State) -> MainOutcome {
    let unexpected = lua_costatus(GL, L) == LUA_COSUS && !SCHED.with(|s| s.borrow().parked.contains(&(L as usize)));
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.main = L as usize;
        s.main_failed = false;
    });
    loop {
        // deferred work first, in order (more may be added meanwhile)
        while let Some(e) = SCHED.with(|s| s.borrow_mut().deferred.pop_front()) {
            run_entry(GL, &e);
        }
        // the earliest timer
        let next = SCHED.with(|s| {
            let s = s.borrow();
            s.timers.iter().map(|e| e.due).fold(None, |m: Option<f64>, d| Some(m.map_or(d, |m| m.min(d))))
        });
        let Some(due) = next else { break };
        let wait = due - now();
        if wait > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(wait.min(1.0e7)));
        }
        // everything due by now, in order of due time then scheduling
        let t = now();
        let mut ready: Vec<Entry> = SCHED.with(|s| {
            let mut s = s.borrow_mut();
            let (ready, rest): (Vec<Entry>, Vec<Entry>) = s.timers.drain(..).partition(|e| e.due <= t);
            s.timers = rest;
            ready
        });
        ready.sort_by(|a, b| a.due.total_cmp(&b.due).then(a.seq.cmp(&b.seq)));
        for e in &ready {
            run_entry(GL, e);
        }
    }
    let failed = SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.main = 0;
        s.main_failed
    });
    if failed {
        MainOutcome::Error
    } else if unexpected {
        MainOutcome::Unexpected
    } else {
        MainOutcome::Done
    }
}

/// Resumes the thread of a due entry (if still suspended) and frees it.
unsafe fn run_entry(GL: *mut lua_State, e: &Entry) {
    lua_rawgeti(GL, LUA_REGISTRYINDEX, e.thread);
    let co = lua_tothread(GL, -1);
    if !co.is_null() && is_suspended(GL, co) {
        let nargs = match e.waitstart {
            Some(start) => {
                lua_pushnumber(co, now() - start);
                1
            }
            None if e.args != LUA_NOREF => unpack_onto(co, e.args),
            None => 0,
        };
        resume(GL, co, nargs);
    }
    lua_pop(GL, 1);
    release(GL, e);
}
