//! Executable memory for native code (Windows x64).
//!
//! One region is reserved per process. Its first page holds the unwind data
//! and the shared entry stub; native functions are appended after it. A single
//! `RUNTIME_FUNCTION` covers the whole region and describes the stub's
//! prologue, which every native function runs under (they are entered through
//! the stub), so Windows can unwind through native frames — needed when an
//! error is raised by unwinding (see `ldo.rs`) or a Rust panic passes through.

use std::sync::Mutex;

#[allow(non_camel_case_types, non_snake_case)]
#[repr(C)]
struct RUNTIME_FUNCTION {
    BeginAddress: u32,
    EndAddress: u32,
    UnwindData: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(addr: *mut u8, size: usize, alloc_type: u32, protect: u32) -> *mut u8;
    fn FlushInstructionCache(process: *mut u8, base: *const u8, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut u8;
    fn RtlAddFunctionTable(table: *const RUNTIME_FUNCTION, count: u32, base: u64) -> u8;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Size of the reserved region.
const REGION: usize = 256 << 20;
/// Commit granularity.
const COMMIT: usize = 1 << 20;

pub struct CodeMemory {
    base: *mut u8,
    /// bytes committed
    committed: usize,
    /// next free byte
    top: usize,
    /// address of the shared entry stub
    pub enter: usize,
    /// freed blocks (offset, size), reused first-fit
    free: Vec<(usize, usize)>,
}

unsafe impl Send for CodeMemory {}

pub static CODE: Mutex<Option<CodeMemory>> = Mutex::new(None);

/// Total bytes of native code placed so far (`luau-compile --codegennull`).
pub static PLACED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Size of the entry stub's prologue (see `emit_enter_stub`).
pub const STUB_PROLOGUE: u8 = 14;

impl CodeMemory {
    unsafe fn create() -> Option<CodeMemory> {
        let base = VirtualAlloc(std::ptr::null_mut(), REGION, MEM_RESERVE, PAGE_EXECUTE_READWRITE);
        if base.is_null() {
            return None;
        }
        if VirtualAlloc(base, COMMIT, MEM_COMMIT, PAGE_EXECUTE_READWRITE).is_null() {
            return None;
        }
        let mut m = CodeMemory { base, committed: COMMIT, top: 0, enter: 0, free: Vec::new() };

        // unwind info for the stub prologue:
        //   push rbx; push rbp; push r12; push r13; push r14; push r15; sub rsp, 8
        // (codes in reverse order: offset-of-end, op | info << 4)
        const PUSH: u8 = 0; // UWOP_PUSH_NONVOL
        const ALLOC_SMALL: u8 = 2;
        let codes: [(u8, u8); 7] = [
            (STUB_PROLOGUE, ALLOC_SMALL), // 8 bytes: info = (8 - 8) / 8 = 0
            (10, PUSH | (15 << 4)),
            (8, PUSH | (14 << 4)),
            (6, PUSH | (13 << 4)),
            (4, PUSH | (12 << 4)),
            (2, PUSH | (5 << 4)),
            (1, PUSH | (3 << 4)),
        ];
        let mut unwind = vec![1u8 /* version 1, no flags */, STUB_PROLOGUE, codes.len() as u8, 0];
        for (off, op) in codes {
            unwind.push(off);
            unwind.push(op);
        }
        unwind.extend_from_slice(&[0, 0]); // pad CountOfCodes to even
        let unwind_off = 64usize;
        std::ptr::copy_nonoverlapping(unwind.as_ptr(), base.add(unwind_off), unwind.len());

        // the stub, then the function table entry covering the whole region
        let stub_off = 256usize;
        let stub = super::x64::emit_enter_stub();
        std::ptr::copy_nonoverlapping(stub.as_ptr(), base.add(stub_off), stub.len());
        m.enter = base as usize + stub_off;
        m.top = (stub_off + stub.len() + 63) & !63;

        let rf = base as *mut RUNTIME_FUNCTION;
        *rf = RUNTIME_FUNCTION { BeginAddress: stub_off as u32, EndAddress: REGION as u32, UnwindData: unwind_off as u32 };
        if RtlAddFunctionTable(rf, 1, base as u64) == 0 {
            return None;
        }
        FlushInstructionCache(GetCurrentProcess(), base, COMMIT);
        Some(m)
    }

    /// Copies `code` into executable memory; returns its address.
    pub unsafe fn place(&mut self, code: &[u8]) -> Option<usize> {
        PLACED.fetch_add(code.len(), std::sync::atomic::Ordering::Relaxed);
        let need = (code.len() + 31) & !31;
        if let Some(k) = self.free.iter().position(|&(_, size)| size >= need) {
            let (off, size) = self.free[k];
            if size > need {
                self.free[k] = (off + need, size - need);
            } else {
                self.free.swap_remove(k);
            }
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.base.add(off), code.len());
            FlushInstructionCache(GetCurrentProcess(), self.base.add(off), code.len());
            return Some(self.base as usize + off);
        }
        let start = (self.top + 31) & !31;
        let end = start + code.len();
        if end > REGION {
            return None;
        }
        while end > self.committed {
            if VirtualAlloc(self.base.add(self.committed), COMMIT, MEM_COMMIT, PAGE_EXECUTE_READWRITE).is_null() {
                return None;
            }
            self.committed += COMMIT;
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), self.base.add(start), code.len());
        FlushInstructionCache(GetCurrentProcess(), self.base.add(start), code.len());
        self.top = end;
        Some(self.base as usize + start)
    }
}

impl CodeMemory {
    /// Returns a block from `place` (size = the code length it was given).
    pub fn release(&mut self, addr: usize, len: usize) {
        let off = addr - self.base as usize;
        let size = (len + 31) & !31;
        // merge with an adjacent free block when possible
        if let Some(k) = self.free.iter().position(|&(o, sz)| o + sz == off) {
            self.free[k].1 += size;
        } else if let Some(k) = self.free.iter().position(|&(o, _)| off + size == o) {
            self.free[k] = (off, self.free[k].1 + size);
        } else {
            self.free.push((off, size));
        }
    }
}

/// Runs `f` with the process-wide code memory (created on first use).
pub fn with_code<T>(f: impl FnOnce(&mut CodeMemory) -> Option<T>) -> Option<T> {
    let mut g = CODE.lock().ok()?;
    if g.is_none() {
        *g = unsafe { CodeMemory::create() };
    }
    f(g.as_mut()?)
}

/// Address of the entry stub (code memory must exist).
pub fn enter_stub() -> usize {
    with_code(|m| Some(m.enter)).unwrap_or(0)
}
