//! Development aid (`LUAU_CRASHINFO=1`, Windows x64): prints the faulting
//! address and registers on an access violation, so crashes in generated
//! code can be located without a debugger.

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: usize,
        nparams: u32,
        info: [usize; 15],
    }
    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut u8,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32) -> *mut u8;
    }

    unsafe extern "system" fn handler(p: *mut ExceptionPointers) -> i32 {
        let rec = &*(*p).record;
        if rec.code == 0xC000_0005 {
            // CONTEXT: integer registers start at offset 0x78 (rax, rcx, rdx,
            // rbx, rsp, rbp, rsi, rdi, r8..r15), rip at 0xf8
            let ctx = (*p).context;
            let reg = |i: usize| *(ctx.add(0x78 + i * 8) as *const u64);
            let names = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
            eprintln!("access violation at rip={:#x} address={:#x}", rec.address, rec.info[1]);
            for (i, n) in names.iter().enumerate() {
                eprint!("{}={:#x} ", n, reg(i));
            }
            eprintln!();
            let rip = rec.address as *const u8;
            eprint!("code:");
            for i in 0..16 {
                eprint!(" {:02x}", *rip.add(i));
            }
            eprintln!();
            let rsp = reg(4) as *const u64;
            eprint!("stack:");
            for i in 0..24 {
                eprint!(" {:#x}", *rsp.add(i));
            }
            eprintln!();
            std::process::exit(139);
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }

    pub fn install() {
        unsafe {
            AddVectoredExceptionHandler(1, handler);
        }
    }
}

pub fn install() {
    if std::env::var_os("LUAU_CRASHINFO").is_some() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        imp::install();
    }
}
