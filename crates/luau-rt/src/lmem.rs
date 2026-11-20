//! Port of `VM/src/lmem.cpp`: the size-class page allocator.
//!
//! Blocks of up to 1 KiB come from 16/32 KiB pages, one size class per page.
//! Non-GC blocks carry an 8-byte header pointing at their page; GC objects
//! do not (the sweeper walks GC pages directly, so it always knows the page
//! of the object it frees). Larger requests go to the system allocator (GC
//! objects get a dedicated single-block page so the sweeper still sees them).
//! Byte accounting (`totalbytes`, `memcatbytes`) follows C++ exactly.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr;

use crate::ldebug::luaG_runerror;
use crate::ldo::luaD_throw;
use crate::lobject::*;

pub const LUA_SIZECLASSES: usize = 40;

const K_MAX_SMALL_SIZE: usize = 1024;
const K_LARGE_PAGE_THRESHOLD: usize = 512;
const K_EXTERNAL_ALLOCATOR_METADATA_REDUCTION: usize = 24;
const K_SMALL_PAGE_SIZE: usize = 16 * 1024 - K_EXTERNAL_ALLOCATOR_METADATA_REDUCTION;
const K_LARGE_PAGE_SIZE: usize = 32 * 1024 - K_EXTERNAL_ALLOCATOR_METADATA_REDUCTION;
const K_BLOCK_HEADER: usize = 8;
/// GC pages keep the free list link right after the GC header.
const K_GCO_LINK_OFFSET: usize = (std::mem::size_of::<GCheader>() + 7) & !7;
const ALIGN: usize = 16;

#[repr(C, align(16))]
pub struct lua_Page {
    /// list of pages with free blocks
    pub prev: *mut lua_Page,
    pub next: *mut lua_Page,
    /// list of all pages
    pub listprev: *mut lua_Page,
    pub listnext: *mut lua_Page,
    pub page_size: i32,
    pub block_size: i32,
    pub free_list: *mut u8,
    pub free_next: i32,
    pub busy_blocks: i32,
}

const PAGE_HEADER: usize = std::mem::size_of::<lua_Page>();

#[inline(always)]
unsafe fn page_data(p: *mut lua_Page) -> *mut u8 {
    (p as *mut u8).add(PAGE_HEADER)
}

struct SizeClassConfig {
    size_of_class: [i32; LUA_SIZECLASSES],
    class_for_size: [i8; K_MAX_SMALL_SIZE + 1],
}

const fn build_config() -> SizeClassConfig {
    let mut size_of_class = [0i32; LUA_SIZECLASSES];
    let mut class_for_size = [-1i8; K_MAX_SMALL_SIZE + 1];
    let mut count = 0;
    let mut size = 8;
    while size < 64 {
        size_of_class[count] = size;
        count += 1;
        size += 8;
    }
    size = 64;
    while size < 256 {
        size_of_class[count] = size;
        count += 1;
        size += 16;
    }
    size = 256;
    while size < 512 {
        size_of_class[count] = size;
        count += 1;
        size += 32;
    }
    size = 512;
    while size <= 1024 {
        size_of_class[count] = size;
        count += 1;
        size += 64;
    }
    let mut k = 0;
    while k < count {
        class_for_size[size_of_class[k] as usize] = k as i8;
        k += 1;
    }
    let mut s = K_MAX_SMALL_SIZE as isize - 1;
    while s >= 0 {
        if class_for_size[s as usize] < 0 {
            class_for_size[s as usize] = class_for_size[s as usize + 1];
        }
        s -= 1;
    }
    SizeClassConfig { size_of_class, class_for_size }
}

static CONFIG: SizeClassConfig = build_config();

/// Size class of a block; -1 for empty or large blocks.
#[inline(always)]
fn sizeclass(sz: usize) -> i32 {
    if sz.wrapping_sub(1) < K_MAX_SMALL_SIZE {
        CONFIG.class_for_size[sz] as i32
    } else {
        -1
    }
}

#[inline(always)]
fn sys_layout(size: usize) -> Layout {
    // SAFETY: ALIGN is a power of two; sizes are bounded by callers.
    unsafe { Layout::from_size_align_unchecked(size.max(1), ALIGN) }
}

#[inline(always)]
unsafe fn over_limit(g: *mut global_State, nsize: usize) -> bool {
    ((*g).memlimit != 0 && nsize > (*g).memlimit) || (*g).allocblocked
}

#[inline(always)]
unsafe fn sys_alloc(g: *mut global_State, size: usize) -> *mut u8 {
    if over_limit(g, size) {
        return ptr::null_mut();
    }
    std::alloc::alloc(sys_layout(size))
}

#[inline(always)]
unsafe fn sys_free(p: *mut u8, size: usize) {
    std::alloc::dealloc(p, sys_layout(size))
}

pub unsafe fn luaM_toobig(L: *mut lua_State) -> ! {
    luaG_runerror(L, "memory allocation error: block too big")
}

unsafe fn newpage(L: *mut lua_State, pageset: *mut *mut lua_Page, page_size: usize, block_size: usize, block_count: usize, nothrow: bool) -> *mut lua_Page {
    let g = (*L).global;
    let page = sys_alloc(g, page_size) as *mut lua_Page;
    if page.is_null() {
        if nothrow {
            return page;
        }
        luaD_throw(L, LUA_ERRMEM);
    }
    (*page).prev = ptr::null_mut();
    (*page).next = ptr::null_mut();
    (*page).listprev = ptr::null_mut();
    (*page).listnext = ptr::null_mut();
    (*page).page_size = page_size as i32;
    (*page).block_size = block_size as i32;
    // start with the last block and move downward
    (*page).free_list = ptr::null_mut();
    (*page).free_next = ((block_count - 1) * block_size) as i32;
    (*page).busy_blocks = 0;
    if !pageset.is_null() {
        (*page).listnext = *pageset;
        if !(*page).listnext.is_null() {
            (*(*page).listnext).listprev = page;
        }
        *pageset = page;
    }
    page
}

#[inline(never)]
unsafe fn newclasspage(
    L: *mut lua_State,
    freepageset: *mut *mut lua_Page,
    pageset: *mut *mut lua_Page,
    size_class: usize,
    store_metadata: bool,
    nothrow: bool,
) -> *mut lua_Page {
    let size_of_class = CONFIG.size_of_class[size_class] as usize;
    let page_size = if size_of_class > K_LARGE_PAGE_THRESHOLD { K_LARGE_PAGE_SIZE } else { K_SMALL_PAGE_SIZE };
    let block_size = size_of_class + if store_metadata { K_BLOCK_HEADER } else { 0 };
    let block_count = (page_size - PAGE_HEADER) / block_size;
    let page = newpage(L, pageset, page_size, block_size, block_count, nothrow);
    if !page.is_null() {
        *freepageset.add(size_class) = page;
    }
    page
}

unsafe fn freepage(pageset: *mut *mut lua_Page, page: *mut lua_Page) {
    if !pageset.is_null() {
        if !(*page).listnext.is_null() {
            (*(*page).listnext).listprev = (*page).listprev;
        }
        if !(*page).listprev.is_null() {
            (*(*page).listprev).listnext = (*page).listnext;
        } else if *pageset == page {
            *pageset = (*page).listnext;
        }
    }
    sys_free(page as *mut u8, (*page).page_size as usize);
}

unsafe fn freeclasspage(freepageset: *mut *mut lua_Page, pageset: *mut *mut lua_Page, page: *mut lua_Page, size_class: usize) {
    if !(*page).next.is_null() {
        (*(*page).next).prev = (*page).prev;
    }
    if !(*page).prev.is_null() {
        (*(*page).prev).next = (*page).next;
    } else if *freepageset.add(size_class) == page {
        *freepageset.add(size_class) = (*page).next;
    }
    freepage(pageset, page);
}

#[inline(always)]
unsafe fn newblock(L: *mut lua_State, size_class: usize, nothrow: bool) -> *mut u8 {
    let g = (*L).global;
    if (*g).allocblocked {
        return ptr::null_mut();
    }
    let freepages = (*g).freepages.as_mut_ptr();
    let mut page = *freepages.add(size_class);
    if page.is_null() {
        page = newclasspage(L, freepages, ptr::null_mut(), size_class, true, nothrow);
        if page.is_null() {
            return ptr::null_mut();
        }
    }
    let block;
    if (*page).free_next >= 0 {
        block = page_data(page).add((*page).free_next as usize);
        (*page).free_next -= (*page).block_size;
    } else {
        block = (*page).free_list;
        (*page).free_list = *(block as *mut *mut u8);
    }
    (*page).busy_blocks += 1;
    *(block as *mut *mut lua_Page) = page;
    if (*page).free_list.is_null() && (*page).free_next < 0 {
        *freepages.add(size_class) = (*page).next;
        if !(*page).next.is_null() {
            (*(*page).next).prev = ptr::null_mut();
        }
        (*page).next = ptr::null_mut();
    }
    block.add(K_BLOCK_HEADER)
}

#[inline(always)]
unsafe fn newgcoblock(L: *mut lua_State, size_class: usize) -> *mut u8 {
    let g = (*L).global;
    let freepages = (*g).freegcopages.as_mut_ptr();
    let mut page = *freepages.add(size_class);
    if page.is_null() {
        page = newclasspage(L, freepages, ptr::addr_of_mut!((*g).allgcopages), size_class, false, false);
    }
    let block;
    if (*page).free_next >= 0 {
        block = page_data(page).add((*page).free_next as usize);
        (*page).free_next -= (*page).block_size;
    } else {
        block = (*page).free_list;
        (*page).free_list = *(block.add(K_GCO_LINK_OFFSET) as *mut *mut u8);
    }
    (*page).busy_blocks += 1;
    if (*page).free_list.is_null() && (*page).free_next < 0 {
        *freepages.add(size_class) = (*page).next;
        if !(*page).next.is_null() {
            (*(*page).next).prev = ptr::null_mut();
        }
        (*page).next = ptr::null_mut();
    }
    block
}

#[inline(always)]
unsafe fn freeblock(L: *mut lua_State, size_class: usize, block: *mut u8) {
    let g = (*L).global;
    let block = block.sub(K_BLOCK_HEADER);
    let page = *(block as *mut *mut lua_Page);
    let freepages = (*g).freepages.as_mut_ptr();
    if (*page).free_list.is_null() && (*page).free_next < 0 {
        (*page).next = *freepages.add(size_class);
        if !(*page).next.is_null() {
            (*(*page).next).prev = page;
        }
        *freepages.add(size_class) = page;
    }
    *(block as *mut *mut u8) = (*page).free_list;
    (*page).free_list = block;
    (*page).busy_blocks -= 1;
    if (*page).busy_blocks == 0 {
        freeclasspage(freepages, ptr::null_mut(), page, size_class);
    }
}

#[inline(always)]
unsafe fn freegcoblock(L: *mut lua_State, size_class: usize, block: *mut u8, page: *mut lua_Page) {
    let g = (*L).global;
    let freepages = (*g).freegcopages.as_mut_ptr();
    if (*page).free_list.is_null() && (*page).free_next < 0 {
        (*page).next = *freepages.add(size_class);
        if !(*page).next.is_null() {
            (*(*page).next).prev = page;
        }
        *freepages.add(size_class) = page;
    }
    *(block.add(K_GCO_LINK_OFFSET) as *mut *mut u8) = (*page).free_list;
    (*page).free_list = block;
    (*page).busy_blocks -= 1;
    if (*page).busy_blocks == 0 {
        freeclasspage(freepages, ptr::addr_of_mut!((*g).allgcopages), page, size_class);
    }
}

#[inline]
pub unsafe fn luaM_new_(L: *mut lua_State, nsize: usize, memcat: u8) -> *mut u8 {
    let g = (*L).global;
    let nclass = sizeclass(nsize);
    let block = if nclass >= 0 { newblock(L, nclass as usize, false) } else { sys_alloc(g, nsize) };
    if block.is_null() && nsize > 0 {
        luaD_throw(L, LUA_ERRMEM);
    }
    (*g).totalbytes += nsize;
    (*g).memcatbytes[memcat as usize] += nsize;
    block
}

/// `luaM_trynew_`: like `luaM_new_` but returns null on failure.
pub unsafe fn luaM_trynew_(L: *mut lua_State, nsize: usize, memcat: u8) -> *mut u8 {
    let g = (*L).global;
    let nclass = sizeclass(nsize);
    let block = if nclass >= 0 { newblock(L, nclass as usize, true) } else { sys_alloc(g, nsize) };
    if block.is_null() {
        return block;
    }
    (*g).totalbytes += nsize;
    (*g).memcatbytes[memcat as usize] += nsize;
    block
}

#[inline]
pub unsafe fn luaM_free_(L: *mut lua_State, block: *mut u8, osize: usize, memcat: u8) {
    if block.is_null() {
        return;
    }
    let g = (*L).global;
    let oclass = sizeclass(osize);
    if oclass >= 0 {
        freeblock(L, oclass as usize, block);
    } else {
        sys_free(block, osize);
    }
    (*g).totalbytes -= osize;
    (*g).memcatbytes[memcat as usize] -= osize;
}

pub unsafe fn luaM_realloc_(L: *mut lua_State, block: *mut u8, osize: usize, nsize: usize, memcat: u8) -> *mut u8 {
    let g = (*L).global;
    let block = if osize == 0 { ptr::null_mut() } else { block };
    let nclass = sizeclass(nsize);
    let oclass = sizeclass(osize);
    let result;
    if nclass >= 0 || oclass >= 0 {
        result = if nclass >= 0 { newblock(L, nclass as usize, false) } else { sys_alloc(g, nsize) };
        if result.is_null() && nsize > 0 {
            luaD_throw(L, LUA_ERRMEM);
        }
        if osize > 0 && nsize > 0 {
            ptr::copy_nonoverlapping(block, result, osize.min(nsize));
        }
        if oclass >= 0 {
            freeblock(L, oclass as usize, block);
        } else if !block.is_null() {
            sys_free(block, osize);
        }
    } else if nsize == 0 {
        if !block.is_null() {
            sys_free(block, osize);
        }
        result = ptr::null_mut();
    } else if block.is_null() {
        result = sys_alloc(g, nsize);
        if result.is_null() {
            luaD_throw(L, LUA_ERRMEM);
        }
    } else {
        if over_limit(g, nsize) {
            luaD_throw(L, LUA_ERRMEM);
        }
        result = std::alloc::realloc(block, sys_layout(osize), nsize);
        if result.is_null() {
            luaD_throw(L, LUA_ERRMEM);
        }
    }
    (*g).totalbytes = (*g).totalbytes - osize + nsize;
    (*g).memcatbytes[memcat as usize] = (*g).memcatbytes[memcat as usize] - osize + nsize;
    result
}

/// Allocates a collectable object of `nsize` bytes in a GC page. The
/// header's `tt`/`marked`/`memcat` are set by `luaC_init`.
#[inline]
pub unsafe fn luaM_newgco_(L: *mut lua_State, nsize: usize, memcat: u8) -> *mut GCObject {
    debug_assert!(nsize >= K_GCO_LINK_OFFSET + 8);
    let g = (*L).global;
    let nclass = sizeclass(nsize);
    let block = if nclass >= 0 {
        if (*g).allocblocked {
            ptr::null_mut()
        } else {
            newgcoblock(L, nclass as usize)
        }
    } else {
        let page = newpage(L, ptr::addr_of_mut!((*g).allgcopages), PAGE_HEADER + nsize, nsize, 1, false);
        (*page).free_next -= (*page).block_size;
        (*page).busy_blocks += 1;
        page_data(page)
    };
    if block.is_null() {
        luaD_throw(L, LUA_ERRMEM);
    }
    (*g).totalbytes += nsize;
    (*g).memcatbytes[memcat as usize] += nsize;
    block as *mut GCObject
}

/// Frees a collectable object; the sweeper records the object's page in
/// `global_State::freepage` before freeing (C++ passes it explicitly).
#[inline]
pub unsafe fn luaM_freegco_(L: *mut lua_State, o: *mut GCObject, osize: usize, memcat: u8) {
    let g = (*L).global;
    let page = (*g).freepage;
    let oclass = sizeclass(osize);
    if oclass >= 0 {
        (*o).tt = LUA_TNIL as u8;
        freegcoblock(L, oclass as usize, o as *mut u8, page);
    } else {
        freepage(ptr::addr_of_mut!((*g).allgcopages), page);
    }
    (*g).totalbytes -= osize;
    (*g).memcatbytes[memcat as usize] -= osize;
}

/// `luaM_getpagewalkinfo`: (first used block, end, busy blocks, block size).
#[inline(always)]
pub unsafe fn luaM_getpagewalkinfo(page: *mut lua_Page) -> (*mut u8, *mut u8, i32, usize) {
    let block_count = ((*page).page_size as usize - PAGE_HEADER) / (*page).block_size as usize;
    let data = page_data(page);
    let start = data.offset(((*page).free_next + (*page).block_size) as isize);
    let end = data.add(block_count * (*page).block_size as usize);
    (start, end, (*page).busy_blocks, (*page).block_size as usize)
}

#[inline(always)]
pub unsafe fn luaM_getnextpage(page: *mut lua_Page) -> *mut lua_Page {
    (*page).listnext
}

/// `luaM_visitgco`: calls `visitor(page, object)` for every live GC object.
pub unsafe fn luaM_visitgco(L: *mut lua_State, visitor: &mut dyn FnMut(*mut lua_Page, *mut GCObject)) {
    let g = (*L).global;
    let mut curr = (*g).allgcopages;
    while !curr.is_null() {
        let next = (*curr).listnext;
        let (start, end, mut busy, bs) = luaM_getpagewalkinfo(curr);
        let mut pos = start;
        while pos != end && busy > 0 {
            let gco = pos as *mut GCObject;
            pos = pos.add(bs);
            if (*gco).tt as i32 == LUA_TNIL {
                continue;
            }
            busy -= 1;
            // the visitor may free the object (and with the last one, the page)
            visitor(curr, gco);
        }
        curr = next;
    }
}

#[inline(always)]
pub fn luaM_arraysize(n: usize, e: usize) -> usize {
    n.checked_mul(e).unwrap_or(usize::MAX)
}

#[inline]
pub unsafe fn luaM_newarray<T>(L: *mut lua_State, n: usize, memcat: u8) -> *mut T {
    let size = luaM_arraysize(n, std::mem::size_of::<T>());
    if size == usize::MAX {
        luaM_toobig(L);
    }
    luaM_new_(L, size, memcat) as *mut T
}

#[inline]
pub unsafe fn luaM_freearray<T>(L: *mut lua_State, b: *mut T, n: usize, memcat: u8) {
    luaM_free_(L, b as *mut u8, n * std::mem::size_of::<T>(), memcat)
}

#[inline]
pub unsafe fn luaM_reallocarray<T>(L: *mut lua_State, v: *mut T, oldn: usize, n: usize, memcat: u8) -> *mut T {
    let size = luaM_arraysize(n, std::mem::size_of::<T>());
    if size == usize::MAX {
        luaM_toobig(L);
    }
    luaM_realloc_(L, v as *mut u8, oldn * std::mem::size_of::<T>(), size, memcat) as *mut T
}
