//! Development aid: a sampling profiler for Windows (`LUAU_PROFILE=<file>`).
//!
//! A helper thread suspends the interpreter thread every ~100us, records its
//! instruction pointer, and on exit writes `<count> <address>` lines, with
//! addresses rebased to the image's preferred base so `addr2line -f -e
//! luau.exe` can symbolize them.

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    type Handle = *mut core::ffi::c_void;

    #[repr(C, align(16))]
    struct Context {
        bytes: [u8; 1232],
    }

    const CONTEXT_CONTROL: u32 = 0x0010_0001;
    const THREAD_ALL_ACCESS: u32 = 0x001F_FFFF;
    const RIP_OFFSET: usize = 0xF8;
    const FLAGS_OFFSET: usize = 0x30;
    const PREFERRED_BASE: u64 = 0x1_4000_0000;

    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenThread(access: u32, inherit: i32, id: u32) -> Handle;
        fn SuspendThread(h: Handle) -> u32;
        fn ResumeThread(h: Handle) -> u32;
        fn GetThreadContext(h: Handle, ctx: *mut Context) -> i32;
        fn GetModuleHandleW(name: *const u16) -> Handle;
    }

    pub struct Profiler {
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<HashMap<u64, u32>>>,
        out: String,
    }

    impl Profiler {
        /// Starts sampling the calling thread.
        pub fn start(out: String) -> Profiler {
            let tid = unsafe { GetCurrentThreadId() };
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let worker = std::thread::spawn(move || {
                let mut hist: HashMap<u64, u32> = HashMap::new();
                let h = unsafe { OpenThread(THREAD_ALL_ACCESS, 0, tid) };
                if h.is_null() {
                    return hist;
                }
                let base = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
                let mut ctx = Box::new(Context { bytes: [0; 1232] });
                while !s.load(Ordering::Relaxed) {
                    // no allocation while the target is suspended: it may hold
                    // the allocator lock
                    let mut sample = None;
                    unsafe {
                        if SuspendThread(h) != u32::MAX {
                            ctx.bytes[FLAGS_OFFSET..FLAGS_OFFSET + 4].copy_from_slice(&CONTEXT_CONTROL.to_le_bytes());
                            if GetThreadContext(h, &mut *ctx) != 0 {
                                let mut rip = [0u8; 8];
                                rip.copy_from_slice(&ctx.bytes[RIP_OFFSET..RIP_OFFSET + 8]);
                                sample = Some(u64::from_le_bytes(rip));
                            }
                            ResumeThread(h);
                        }
                    }
                    if let Some(rip) = sample {
                        // rebase addresses inside the executable (for addr2line);
                        // others (native code, DLLs) are kept as they are
                        let key = if rip >= base && rip - base < (256 << 20) { rip - base + PREFERRED_BASE } else { rip };
                        *hist.entry(key).or_default() += 1;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                hist
            });
            Profiler { stop, worker: Some(worker), out }
        }
    }

    impl Drop for Profiler {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(w) = self.worker.take() {
                if let Ok(hist) = w.join() {
                    let mut v: Vec<_> = hist.into_iter().collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1));
                    let text: String = v.iter().map(|(a, c)| format!("{} {:#x}\n", c, a)).collect();
                    let _ = std::fs::write(&self.out, text);
                }
            }
        }
    }
}

#[cfg(windows)]
pub use imp::Profiler;

#[cfg(not(windows))]
pub struct Profiler;

#[cfg(not(windows))]
impl Profiler {
    pub fn start(_out: String) -> Profiler {
        Profiler
    }
}
