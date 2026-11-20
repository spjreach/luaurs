//! Port of `CLI/src/Profiler.cpp`: `--profile[=N]` samples Lua call stacks
//! at N Hz through the interrupt callback and writes `profile.out` (one
//! `<microseconds> <stack>` line per distinct stack, as read by
//! `tools/perfgraph.py`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use luau_rt::ldebug::{lua_Debug, lua_getinfo};
use luau_rt::lobject::{lua_Callbacks, lua_State};

static EXIT: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

struct State {
    current_ticks: u64,
    data: HashMap<Vec<u8>, u64>,
    gc: [u64; 16],
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State { current_ticks: 0, data: HashMap::new(), gc: [0; 16] });
}

static mut THREAD: Option<std::thread::JoinHandle<()>> = None;

fn set_interrupt(f: Option<unsafe fn(*mut lua_State, i32)>) {
    let cb = CALLBACKS.load(Ordering::Relaxed) as *mut lua_Callbacks;
    // SAFETY: the callbacks outlive the profiler; like the C++ profiler the
    // interrupt pointer is written from the sampling thread and read by the VM
    unsafe { std::ptr::write_volatile(&mut (*cb).interrupt, f) };
}

/// `profilerTrigger`.
unsafe fn trigger(L: *mut lua_State, gc: i32) {
    let current = TICKS.load(Ordering::Relaxed);
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let elapsed = current - st.current_ticks;
        if elapsed != 0 {
            let mut stack: Vec<u8> = Vec::new();
            if gc > 0 {
                stack.extend_from_slice(b"GC,GC,");
            }
            let mut ar = lua_Debug::default();
            let mut level = 0;
            while lua_getinfo(L, level, b"sn", &mut ar) != 0 {
                if !stack.is_empty() {
                    stack.push(b';');
                }
                stack.extend_from_slice(&ar.short_src);
                stack.push(b',');
                if let Some(name) = &ar.name {
                    stack.extend_from_slice(name);
                }
                stack.push(b',');
                if ar.linedefined > 0 {
                    stack.extend_from_slice(ar.linedefined.to_string().as_bytes());
                }
                level += 1;
            }
            if !stack.is_empty() {
                *st.data.entry(stack).or_insert(0) += elapsed;
            }
            if gc > 0 {
                st.gc[gc as usize & 15] += elapsed;
            }
        }
        st.current_ticks = current;
    });
    set_interrupt(None);
}

/// `profilerLoop`.
fn sampling_loop(frequency: i32) {
    let mut last = luau_rt::los::lua_clock();
    while !EXIT.load(Ordering::Relaxed) {
        let now = luau_rt::los::lua_clock();
        if now - last >= 1.0 / frequency as f64 {
            let ticks = ((now - last) * 1e6) as i64;
            TICKS.fetch_add(ticks as u64, Ordering::Relaxed);
            SAMPLES.fetch_add(1, Ordering::Relaxed);
            set_interrupt(Some(trigger));
            last += ticks as f64 * 1e-6;
        } else {
            std::thread::yield_now();
        }
    }
}

/// `profilerStart`.
pub unsafe fn start(L: *mut lua_State, frequency: i32) {
    CALLBACKS.store(luau_rt::lapi::lua_callbacks(L) as usize, Ordering::Relaxed);
    EXIT.store(false, Ordering::Relaxed);
    let frequency = frequency.max(1);
    THREAD = Some(std::thread::spawn(move || sampling_loop(frequency)));
}

/// `profilerStop`.
pub unsafe fn stop() {
    EXIT.store(true, Ordering::Relaxed);
    #[allow(static_mut_refs)]
    if let Some(t) = THREAD.take() {
        let _ = t.join();
    }
}

/// `profilerDump`.
pub fn dump(path: &str) {
    let Ok(mut f) = std::fs::File::create(path) else {
        eprintln!("Error opening profile {}", path);
        return;
    };
    STATE.with(|st| {
        let st = st.borrow();
        let mut total = 0u64;
        let mut out = Vec::new();
        for (stack, &ticks) in &st.data {
            out.extend_from_slice(ticks.to_string().as_bytes());
            out.push(b' ');
            out.extend_from_slice(stack);
            out.push(b'\n');
            total += ticks;
        }
        let _ = f.write_all(&out);
        println!(
            "Profiler dump written to {} (total runtime {:.3} seconds, {} samples, {} stacks)",
            path,
            total as f64 / 1e6,
            SAMPLES.load(Ordering::Relaxed),
            st.data.len()
        );
        let totalgc: u64 = st.gc.iter().sum();
        if totalgc != 0 {
            print!("GC: {:.3} seconds ({:.2}%)", totalgc as f64 / 1e6, totalgc as f64 / total as f64 * 100.0);
            for (i, &p) in st.gc.iter().enumerate() {
                if p != 0 {
                    let name = luau_rt::lgc::luaC_statename(i as i32).unwrap_or("?");
                    print!(", {} {:.2}%", name, p as f64 / totalgc as f64 * 100.0);
                }
            }
            println!();
        }
    });
}
