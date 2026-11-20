//! Port of `VM/src/lbuflib.cpp` (`LuauIntegerLibrary` on). Little-endian only.

use std::ptr;

use crate::lapi::*;
use crate::laux::*;
use crate::lbaselib::luaL_argcheck;
use crate::lobject::*;

#[inline(always)]
fn isoutofbounds(offset: i32, len: usize, accessize: u64) -> bool {
    (offset as u32 as u64) + accessize > len as u64
}

unsafe fn buffer_create(L: *mut lua_State) -> i32 {
    let size = luaL_checkinteger(L, 1);
    luaL_argcheck(L, size >= 0, 1, "size");
    lua_newbuffer(L, size as usize);
    1
}

unsafe fn buffer_fromstring(L: *mut lua_State) -> i32 {
    let s = luaL_checklstring(L, 1);
    let (p, n) = (s.as_ptr(), s.len());
    let data = lua_newbuffer(L, n);
    ptr::copy_nonoverlapping(p, data, n);
    1
}

unsafe fn buffer_tostring(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    lua_pushlstring(L, std::slice::from_raw_parts(buf, len));
    1
}

trait BufNum: Copy {
    const SIZE: usize;
    fn to_f64(self) -> f64;
    fn from_u32(v: u32) -> Self;
    fn from_f64(v: f64) -> Self;
}

macro_rules! bufint {
    ($t:ty) => {
        impl BufNum for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn from_u32(v: u32) -> Self {
                v as $t
            }
            #[inline(always)]
            fn from_f64(v: f64) -> Self {
                v as $t
            }
        }
    };
}
bufint!(i8);
bufint!(u8);
bufint!(i16);
bufint!(u16);
bufint!(i32);
bufint!(u32);
bufint!(f32);
bufint!(f64);

unsafe fn buffer_read<T: BufNum>(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    if isoutofbounds(offset, len, T::SIZE as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    let v = ptr::read_unaligned(buf.add(offset as usize) as *const T);
    lua_pushnumber(L, v.to_f64());
    1
}

unsafe fn buffer_writeint<T: BufNum>(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let value = luaL_checkunsigned(L, 3);
    if isoutofbounds(offset, len, T::SIZE as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::write_unaligned(buf.add(offset as usize) as *mut T, T::from_u32(value));
    0
}

unsafe fn buffer_writefp<T: BufNum>(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let value = luaL_checknumber(L, 3);
    if isoutofbounds(offset, len, T::SIZE as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::write_unaligned(buf.add(offset as usize) as *mut T, T::from_f64(value));
    0
}

unsafe fn buffer_readlong(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    if isoutofbounds(offset, len, 8) {
        luaL_error(L, "buffer access out of bounds");
    }
    let v = ptr::read_unaligned(buf.add(offset as usize) as *const i64);
    lua_pushinteger64(L, v);
    1
}

unsafe fn buffer_writelong(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let value = luaL_checkinteger64(L, 3);
    if isoutofbounds(offset, len, 8) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::write_unaligned(buf.add(offset as usize) as *mut i64, value);
    0
}

unsafe fn buffer_readstring(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let size = luaL_checkinteger(L, 3);
    luaL_argcheck(L, size >= 0, 3, "size");
    if isoutofbounds(offset, len, size as u32 as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    lua_pushlstring(L, std::slice::from_raw_parts(buf.add(offset as usize), size as usize));
    1
}

unsafe fn buffer_writestring(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let val = luaL_checklstring(L, 3);
    let size = val.len();
    let count = luaL_optinteger(L, 4, size as i32);
    luaL_argcheck(L, count >= 0, 4, "count");
    if count as usize > size {
        luaL_error(L, "string length overflow");
    }
    if isoutofbounds(offset, len, count as u32 as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::copy_nonoverlapping(val.as_ptr(), buf.add(offset as usize), count as usize);
    0
}

unsafe fn buffer_len(L: *mut lua_State) -> i32 {
    let (_, len) = luaL_checkbuffer(L, 1);
    lua_pushnumber(L, len as u32 as f64);
    1
}

unsafe fn buffer_copy(L: *mut lua_State) -> i32 {
    let (tbuf, tlen) = luaL_checkbuffer(L, 1);
    let toffset = luaL_checkinteger(L, 2);
    let (sbuf, slen) = luaL_checkbuffer(L, 3);
    let soffset = luaL_optinteger(L, 4, 0);
    let size = luaL_optinteger(L, 5, (slen as i32).wrapping_sub(soffset));
    if size < 0 {
        luaL_error(L, "buffer access out of bounds");
    }
    if isoutofbounds(soffset, slen, size as u32 as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    if isoutofbounds(toffset, tlen, size as u32 as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::copy(sbuf.add(soffset as usize), tbuf.add(toffset as usize), size as usize);
    0
}

unsafe fn buffer_fill(L: *mut lua_State) -> i32 {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let offset = luaL_checkinteger(L, 2);
    let value = luaL_checkunsigned(L, 3);
    let size = luaL_optinteger(L, 4, (len as i32).wrapping_sub(offset));
    if size < 0 {
        luaL_error(L, "buffer access out of bounds");
    }
    if isoutofbounds(offset, len, size as u32 as u64) {
        luaL_error(L, "buffer access out of bounds");
    }
    ptr::write_bytes(buf.add(offset as usize), (value & 0xff) as u8, size as usize);
    0
}

unsafe fn bits_args(L: *mut lua_State) -> (*mut u8, i64, i32, usize, usize) {
    let (buf, len) = luaL_checkbuffer(L, 1);
    let bitoffset = luaL_checknumber(L, 2) as i64;
    let bitcount = luaL_checkinteger(L, 3);
    if bitoffset < 0 {
        luaL_error(L, "buffer access out of bounds");
    }
    if bitcount as u32 > 32 {
        luaL_error(L, "bit count is out of range of [0; 32]");
    }
    if (bitoffset + bitcount as i64) as u64 > (len as u64) * 8 {
        luaL_error(L, "buffer access out of bounds");
    }
    let startbyte = (bitoffset / 8) as u32 as usize;
    let endbyte = ((bitoffset + bitcount as i64 + 7) / 8) as u32 as usize;
    (buf, bitoffset, bitcount, startbyte, endbyte)
}

unsafe fn buffer_readbits(L: *mut lua_State) -> i32 {
    let (buf, bitoffset, bitcount, startbyte, endbyte) = bits_args(L);
    let mut data: u64 = 0;
    ptr::copy_nonoverlapping(buf.add(startbyte), &mut data as *mut u64 as *mut u8, endbyte - startbyte);
    let sub = (bitoffset & 7) as u32;
    let mask = (1u64 << bitcount) - 1;
    lua_pushunsigned(L, ((data >> sub) & mask) as u32);
    1
}

unsafe fn buffer_writebits(L: *mut lua_State) -> i32 {
    let (buf, bitoffset, bitcount, startbyte, endbyte) = bits_args(L);
    let value = luaL_checkunsigned(L, 4);
    let mut data: u64 = 0;
    ptr::copy_nonoverlapping(buf.add(startbyte), &mut data as *mut u64 as *mut u8, endbyte - startbyte);
    let sub = (bitoffset & 7) as u32;
    let mask = ((1u64 << bitcount) - 1) << sub;
    data = (data & !mask) | (((value as u64) << sub) & mask);
    ptr::copy_nonoverlapping(&data as *const u64 as *const u8, buf.add(startbyte), endbyte - startbyte);
    0
}

const BUFFERLIB: &[luaL_Reg] = &[
    ("create", buffer_create),
    ("fromstring", buffer_fromstring),
    ("tostring", buffer_tostring),
    ("readi8", buffer_read::<i8>),
    ("readu8", buffer_read::<u8>),
    ("readi16", buffer_read::<i16>),
    ("readu16", buffer_read::<u16>),
    ("readi32", buffer_read::<i32>),
    ("readu32", buffer_read::<u32>),
    ("readf32", buffer_read::<f32>),
    ("readf64", buffer_read::<f64>),
    ("writei8", buffer_writeint::<i8>),
    ("writeu8", buffer_writeint::<u8>),
    ("writei16", buffer_writeint::<i16>),
    ("writeu16", buffer_writeint::<u16>),
    ("writei32", buffer_writeint::<i32>),
    ("writeu32", buffer_writeint::<u32>),
    ("writef32", buffer_writefp::<f32>),
    ("writef64", buffer_writefp::<f64>),
    ("readstring", buffer_readstring),
    ("writestring", buffer_writestring),
    ("len", buffer_len),
    ("copy", buffer_copy),
    ("fill", buffer_fill),
    ("readbits", buffer_readbits),
    ("writebits", buffer_writebits),
    ("readinteger", buffer_readlong),
    ("writeinteger", buffer_writelong),
];

pub unsafe fn luaopen_buffer(L: *mut lua_State) -> i32 {
    luaL_register(L, Some("buffer"), BUFFERLIB);
    1
}
