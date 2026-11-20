//! A small-object allocator tuned for the interpreter's allocation pattern.
//!
//! The VM allocates and frees enormous numbers of tiny, short-lived blocks
//! (argument vectors, scopes, strings). The platform allocator — the Windows
//! process heap in particular — spends 50-100ns on each such round trip.
//! This allocator serves every request of up to [`MAX_SMALL`] bytes from
//! per-size-class free lists carved out of large chunks, which makes the
//! common path a handful of instructions. Larger requests go straight to the
//! system allocator.
//!
//! Like `luaM_` in the reference VM (`lmem.cpp`), freed small blocks are
//! recycled within their size class and their pages are never returned to the
//! OS while the process lives.
//!
//! Thread safety: each thread has its own pools (native thread-local
//! storage), so the hot path takes no lock. A block freed by a thread other
//! than the one that allocated it simply joins the freeing thread's free list
//! of the same size class, which is sound because all blocks of a class are
//! interchangeable. Pools of exited threads are not reclaimed (like the
//! process-lifetime chunks themselves).

use std::alloc::{GlobalAlloc, Layout, System};

/// Largest request served from the size classes.
pub const MAX_SMALL: usize = 1024;
/// Size-class granularity (also the alignment every small block gets).
const GRANULE: usize = 16;
const CLASSES: usize = MAX_SMALL / GRANULE;
/// Size of the chunks requested from the system allocator.
const CHUNK: usize = 1 << 20;

/// Largest request cached in the medium classes (bigger ones go straight
/// to the system allocator).
pub const MAX_MEDIUM: usize = 4 << 20;
/// Medium classes: four per power of two over (1KB, 4MB].
const MEDIUM_CLASSES: usize = 48;
/// Most bytes of freed medium blocks a thread keeps for reuse.
const MEDIUM_CACHE_CAP: usize = 64 << 20;

struct Pools {
    /// Head of the free list of each size class (intrusive singly linked).
    free: [*mut u8; CLASSES],
    /// Free lists of the medium classes. Medium blocks come from the system
    /// allocator one by one; keeping freed ones avoids handing them back to
    /// the OS (Windows decommits big heap blocks, so reusing a fresh one
    /// costs page faults: per-compile hash tables, `table.create` arrays).
    medium_free: [*mut u8; MEDIUM_CLASSES],
    /// Bytes currently held in `medium_free`.
    medium_cached: usize,
    /// Unused tail of the current chunk.
    bump: *mut u8,
    bump_end: *mut u8,
}

#[thread_local]
static mut POOLS: Pools = Pools {
    free: [std::ptr::null_mut(); CLASSES],
    medium_free: [std::ptr::null_mut(); MEDIUM_CLASSES],
    medium_cached: 0,
    bump: std::ptr::null_mut(),
    bump_end: std::ptr::null_mut(),
};

#[inline(always)]
fn pools() -> &'static mut Pools {
    // SAFETY: thread-local, and the allocator never re-enters itself
    unsafe { &mut *std::ptr::addr_of_mut!(POOLS) }
}

/// Size class of a small request (`0` is the 16-byte class).
#[inline(always)]
fn class_of(size: usize) -> usize {
    (size.max(1) + GRANULE - 1) / GRANULE - 1
}

#[inline(always)]
fn is_small(layout: &Layout) -> bool {
    layout.size() <= MAX_SMALL && layout.align() <= GRANULE
}

#[inline(always)]
fn is_medium(layout: &Layout) -> bool {
    layout.size() <= MAX_MEDIUM && layout.align() <= GRANULE
}

/// Medium class of a request in (MAX_SMALL, MAX_MEDIUM]: (index, class size).
#[inline(always)]
fn medium_class(size: usize) -> (usize, usize) {
    let e = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize; // 2^e < size <= 2^(e+1)
    let step = 1usize << (e - 2);
    let k = (size - (1 << e)).div_ceil(step); // 1..=4
    ((e - 10) * 4 + (k - 1), (1 << e) + k * step)
}

#[inline(never)]
unsafe fn medium_alloc(size: usize) -> *mut u8 {
    let (idx, cs) = medium_class(size);
    let pools = pools();
    let head = pools.medium_free[idx];
    if !head.is_null() {
        pools.medium_free[idx] = *(head as *mut *mut u8);
        pools.medium_cached -= cs;
        return head;
    }
    System.alloc(Layout::from_size_align_unchecked(cs, GRANULE))
}

#[inline(never)]
unsafe fn medium_dealloc(ptr: *mut u8, size: usize) {
    let (idx, cs) = medium_class(size);
    let pools = pools();
    if pools.medium_cached + cs <= MEDIUM_CACHE_CAP {
        *(ptr as *mut *mut u8) = pools.medium_free[idx];
        pools.medium_free[idx] = ptr;
        pools.medium_cached += cs;
    } else {
        System.dealloc(ptr, Layout::from_size_align_unchecked(cs, GRANULE));
    }
}

/// Which pool a layout belongs to (blocks never move between kinds).
#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    Small(usize),
    Medium(usize),
    Large,
}

#[inline(always)]
fn kind_of(layout: &Layout) -> Kind {
    if is_small(layout) {
        Kind::Small(class_of(layout.size()))
    } else if is_medium(layout) {
        Kind::Medium(medium_class(layout.size()).0)
    } else {
        Kind::Large
    }
}

#[cold]
unsafe fn refill(pools: &mut Pools, bytes: usize) -> *mut u8 {
    // The tail of the previous chunk is abandoned; it is at most one block.
    let chunk = System.alloc(Layout::from_size_align_unchecked(CHUNK, GRANULE));
    if chunk.is_null() {
        return chunk;
    }
    pools.bump = chunk.add(bytes);
    pools.bump_end = chunk.add(CHUNK);
    chunk
}

// ---------------------------------------------------------------------------
// Scoped bump arena (used for the parser's AST)
// ---------------------------------------------------------------------------
//
// While an arena is "on" for a thread, that thread's allocations (alignment
// <= 16) are bump-allocated from large chunks, and frees of arena memory are
// no-ops. `arena_reset` releases everything at once. The compiler parses into
// the arena and, after compiling, forgets the AST and resets the arena instead
// of freeing tens of thousands of nodes one by one (the C++ parser uses an
// arena allocator the same way).

const ARENA_CHUNK: usize = 1 << 20;
const ARENA_MAX_CHUNKS: usize = 64;

struct Arena {
    /// allocations go to the arena
    on: bool,
    /// the arena holds chunks (frees must check for arena pointers)
    live: bool,
    n: usize,
    chunks: [(usize, usize); ARENA_MAX_CHUNKS],
    bump: usize,
    end: usize,
    /// last allocation (for in-place growth of the most recent block)
    last: usize,
    /// a released standard-size chunk kept for the next arena
    spare: usize,
}

#[thread_local]
static mut ARENA: Arena = Arena {
    on: false,
    live: false,
    n: 0,
    chunks: [(0, 0); ARENA_MAX_CHUNKS],
    bump: 0,
    end: 0,
    last: 0,
    spare: 0,
};

#[inline(always)]
fn arena() -> &'static mut Arena {
    // SAFETY: thread-local, and the allocator never re-enters itself
    unsafe { &mut *std::ptr::addr_of_mut!(ARENA) }
}

#[inline(always)]
fn in_arena(a: &Arena, p: usize) -> bool {
    a.live && a.chunks[..a.n].iter().any(|&(s, e)| p >= s && p < e)
}

#[inline(never)]
unsafe fn arena_alloc(a: &mut Arena, size: usize) -> *mut u8 {
    let p = (a.bump + (GRANULE - 1)) & !(GRANULE - 1);
    if a.bump != 0 && p + size <= a.end {
        a.bump = p + size;
        a.last = p;
        return p as *mut u8;
    }
    if a.n == ARENA_MAX_CHUNKS {
        // out of chunk slots: stop using the arena for this scope
        a.on = false;
        return std::ptr::null_mut();
    }
    let csize = if size + GRANULE > ARENA_CHUNK { size + GRANULE } else { ARENA_CHUNK };
    let chunk = if csize == ARENA_CHUNK && a.spare != 0 {
        std::mem::replace(&mut a.spare, 0) as *mut u8
    } else {
        System.alloc(Layout::from_size_align_unchecked(csize, GRANULE))
    };
    if chunk.is_null() {
        a.on = false;
        return chunk;
    }
    let c = chunk as usize;
    a.chunks[a.n] = (c, c + csize);
    a.n += 1;
    a.live = true;
    a.bump = c + size;
    a.end = c + csize;
    a.last = c;
    chunk
}

/// Starts bump-allocating this thread's allocations. Returns false (and does
/// nothing) when [`FastAlloc`] is not the global allocator or an arena is
/// already active; the caller must then free normally.
pub fn arena_begin() -> bool {
    let a = arena();
    if a.on || a.live {
        return false;
    }
    a.on = true;
    // probe: is FastAlloc actually serving this allocation?
    let probe = Box::new(0u64);
    let ok = in_arena(arena(), &*probe as *const u64 as usize);
    drop(probe);
    if !ok {
        arena().on = false;
    }
    ok
}

/// Stops bump-allocating; arena memory stays valid until [`arena_reset`].
pub fn arena_end() {
    arena().on = false;
}

/// Releases all arena memory at once.
///
/// # Safety
/// Nothing allocated while the arena was on may be used afterwards (objects
/// in it must have been forgotten or dropped already).
pub unsafe fn arena_reset() {
    let a = arena();
    a.on = false;
    for i in 0..a.n {
        let (s, e) = a.chunks[i];
        if e - s == ARENA_CHUNK && a.spare == 0 {
            a.spare = s;
        } else {
            System.dealloc(s as *mut u8, Layout::from_size_align_unchecked(e - s, GRANULE));
        }
    }
    a.n = 0;
    a.live = false;
    a.bump = 0;
    a.end = 0;
    a.last = 0;
}

/// The global allocator type; install with `#[global_allocator]`.
pub struct FastAlloc;

unsafe impl GlobalAlloc for FastAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let a = arena();
        if a.on && layout.align() <= GRANULE {
            let p = arena_alloc(a, layout.size());
            if !p.is_null() {
                return p;
            }
        }
        if !is_small(&layout) {
            if is_medium(&layout) {
                return medium_alloc(layout.size());
            }
            return System.alloc(layout);
        }
        let class = class_of(layout.size());
        let pools = pools();
        let head = pools.free[class];
        let block = if !head.is_null() {
            pools.free[class] = *(head as *mut *mut u8);
            head
        } else {
            let bytes = (class + 1) * GRANULE;
            if (pools.bump_end as usize) - (pools.bump as usize) >= bytes {
                let block = pools.bump;
                pools.bump = block.add(bytes);
                block
            } else {
                refill(pools, bytes)
            }
        };
        block
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if in_arena(arena(), ptr as usize) {
            return; // freed with the whole arena
        }
        if !is_small(&layout) {
            if is_medium(&layout) {
                return medium_dealloc(ptr, layout.size());
            }
            return System.dealloc(ptr, layout);
        }
        let class = class_of(layout.size());
        let pools = pools();
        *(ptr as *mut *mut u8) = pools.free[class];
        pools.free[class] = ptr;
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let a = arena();
        if in_arena(a, ptr as usize) {
            // the most recent arena block grows in place when there is room
            if a.on && ptr as usize == a.last && a.last + new_size <= a.end {
                a.bump = a.last + new_size;
                return ptr;
            }
            let fresh = self.alloc(new_layout);
            if !fresh.is_null() {
                std::ptr::copy_nonoverlapping(ptr, fresh, layout.size().min(new_size));
            }
            return fresh;
        }
        let (from, to) = (kind_of(&layout), kind_of(&new_layout));
        if from == to {
            match from {
                // growing or shrinking within a size class needs no move
                Kind::Small(_) | Kind::Medium(_) => return ptr,
                Kind::Large => return System.realloc(ptr, layout, new_size),
            }
        }
        let fresh = self.alloc(new_layout);
        if !fresh.is_null() {
            std::ptr::copy_nonoverlapping(ptr, fresh, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medium_classes() {
        assert_eq!(medium_class(1025), (0, 1280));
        assert_eq!(medium_class(1280), (0, 1280));
        assert_eq!(medium_class(1281), (1, 1536));
        assert_eq!(medium_class(2048), (3, 2048));
        assert_eq!(medium_class(2049), (4, 2560));
        assert_eq!(medium_class(MAX_MEDIUM).0, MEDIUM_CLASSES - 1);
        assert_eq!(medium_class(MAX_MEDIUM).1, MAX_MEDIUM);
    }

    #[test]
    fn size_classes_cover_small_range() {
        assert_eq!(class_of(0), 0);
        assert_eq!(class_of(1), 0);
        assert_eq!(class_of(16), 0);
        assert_eq!(class_of(17), 1);
        assert_eq!(class_of(MAX_SMALL), CLASSES - 1);
    }

    #[test]
    fn blocks_are_recycled_and_distinct() {
        let a = FastAlloc;
        let layout = Layout::from_size_align(40, 8).unwrap();
        unsafe {
            let p1 = a.alloc(layout);
            let p2 = a.alloc(layout);
            assert!(!p1.is_null() && !p2.is_null() && p1 != p2);
            assert_eq!(p1 as usize % GRANULE, 0);
            p1.write_bytes(0xAB, 40);
            p2.write_bytes(0xCD, 40);
            assert_eq!(*p1, 0xAB);
            a.dealloc(p1, layout);
            // Other tests may allocate concurrently, so only check validity.
            let p3 = a.alloc(layout);
            assert!(!p3.is_null() && p3 != p2);
            a.dealloc(p2, layout);
            a.dealloc(p3, layout);
        }
    }

    #[test]
    fn realloc_preserves_contents_across_classes() {
        let a = FastAlloc;
        unsafe {
            let small = Layout::from_size_align(24, 8).unwrap();
            let p = a.alloc(small);
            for i in 0..24 {
                *p.add(i) = i as u8;
            }
            // small -> small (different class) -> large -> small
            let p = a.realloc(p, small, 200);
            let mid = Layout::from_size_align(200, 8).unwrap();
            let p = a.realloc(p, mid, 5000);
            let big = Layout::from_size_align(5000, 8).unwrap();
            let p = a.realloc(p, big, 32);
            for i in 0..24 {
                assert_eq!(*p.add(i), i as u8);
            }
            a.dealloc(p, Layout::from_size_align(32, 8).unwrap());
        }
    }
}
