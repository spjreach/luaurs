//! Port of `VM/src/lvmload.cpp`: `luau_load` deserializes bytecode into
//! protos and pushes the main closure (or an error message).

use crate::lapi::*;
use crate::ldo::*;
use crate::lfunc::*;
use crate::lgc::*;
use crate::lmem::*;
use crate::lobject::*;
use crate::lstring::*;
use crate::ltable::*;
use crate::lvmexecute::luaV_getimport;

const LBC_VERSION_MIN: u8 = 3;
const LBC_VERSION_MAX: u8 = 14;
const LBC_VERSION_CLASSES: u8 = 100;
const LBC_TYPE_VERSION_MIN: u8 = 1;
const LBC_TYPE_VERSION_MAX: u8 = 3;

const LBC_CONSTANT_NIL: u8 = 0;
const LBC_CONSTANT_BOOLEAN: u8 = 1;
const LBC_CONSTANT_NUMBER: u8 = 2;
const LBC_CONSTANT_STRING: u8 = 3;
const LBC_CONSTANT_IMPORT: u8 = 4;
const LBC_CONSTANT_TABLE: u8 = 5;
const LBC_CONSTANT_CLOSURE: u8 = 6;
const LBC_CONSTANT_VECTOR: u8 = 7;
const LBC_CONSTANT_TABLE_WITH_CONSTANTS: u8 = 8;
const LBC_CONSTANT_INTEGER: u8 = 9;
const LBC_CONSTANT_VECTORD: u8 = 11;
const LBC_CONSTANT_CLASS_SHAPE: u8 = 10;

const LPF_INLINABLE: u8 = 1 << 3;
const LFT_CALLTARGET: u8 = 0;
const LFT_CONSTRUCT: u8 = 1;

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn u8(&mut self) -> u8 {
        let v = self.data.get(self.offset).copied().unwrap_or(0);
        self.offset += 1;
        v
    }
    #[inline]
    fn u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        for x in b.iter_mut() {
            *x = self.u8();
        }
        u32::from_le_bytes(b)
    }
    #[inline]
    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
    #[inline]
    fn f32(&mut self) -> f32 {
        f32::from_bits(self.u32())
    }
    #[inline]
    fn f64(&mut self) -> f64 {
        let lo = self.u32() as u64;
        let hi = self.u32() as u64;
        f64::from_bits(lo | (hi << 32))
    }
    fn varint(&mut self) -> u32 {
        let mut result: u32 = 0;
        let mut shift = 0;
        loop {
            let byte = self.u8();
            result |= ((byte & 127) as u32).wrapping_shl(shift);
            shift += 7;
            if byte & 128 == 0 {
                break;
            }
        }
        result
    }
    fn varint64(&mut self) -> u64 {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = self.u8();
            result |= ((byte & 127) as u64).wrapping_shl(shift);
            shift += 7;
            if byte & 128 == 0 {
                break;
            }
        }
        result
    }
}

unsafe fn resolve_import_safe(L: *mut lua_State, k: *mut TValue, id: u32) {
    if (*(*L).gt).safeenv != 0 {
        let oldtop = savestack(L, (*L).top);
        let status = luaD_pcall(
            L,
            &mut || {
                luaD_checkstack(L, 1);
                setnilvalue((*L).top);
                (*L).top = (*L).top.add(1);
                luaV_getimport(L, (*L).gt, k, (*L).top.sub(1), id, true);
            },
            oldtop,
            0,
        );
        if status != 0 {
            setnilvalue((*L).top.sub(1));
        }
    } else {
        setnilvalue((*L).top);
        (*L).top = (*L).top.add(1);
    }
}

unsafe fn loadsafe(L: *mut lua_State, chunkname: &[u8], data: &[u8], env: i32) -> i32 {
    let mut r = Reader { data, offset: 0 };
    let version = r.u8();
    if version == 0 {
        let mut msg = luaO_chunkid(chunkname, LUA_IDSIZE);
        msg.extend_from_slice(&data[1..]);
        lua_pushlstring(L, &msg);
        return 1;
    }
    let chunkid = || String::from_utf8_lossy(&luaO_chunkid(chunkname, LUA_IDSIZE)).into_owned();
    if (version < LBC_VERSION_MIN || version > LBC_VERSION_MAX) && version != LBC_VERSION_CLASSES {
        lua_pushstring(
            L,
            &format!("{}: bytecode version mismatch (expected [{}..{}], got {})", chunkid(), LBC_VERSION_MIN, LBC_VERSION_MAX, version),
        );
        return 1;
    }
    let mut typesversion = 0u8;
    if version >= 4 {
        typesversion = r.u8();
        if !(LBC_TYPE_VERSION_MIN..=LBC_TYPE_VERSION_MAX).contains(&typesversion) {
            lua_pushstring(
                L,
                &format!(
                    "{}: bytecode type version mismatch (expected [{}..{}], got {})",
                    chunkid(),
                    LBC_TYPE_VERSION_MIN,
                    LBC_TYPE_VERSION_MAX,
                    typesversion
                ),
            );
            return 1;
        }
    }

    let envt = if env == 0 { (*L).gt } else { hvalue(luaA_toobject(L, env)) };
    let source = luaS_newlstr(L, chunkname);

    let string_count = r.varint() as usize;
    let mut strings: Vec<*mut TString> = Vec::with_capacity(string_count);
    for _ in 0..string_count {
        let length = r.varint() as usize;
        let s = &data[r.offset.min(data.len())..(r.offset + length).min(data.len())];
        strings.push(luaS_newlstr(L, s));
        r.offset += length;
    }
    let read_string = |r: &mut Reader| -> *mut TString {
        let id = r.varint() as usize;
        if id == 0 {
            std::ptr::null_mut()
        } else {
            strings[id - 1]
        }
    };

    if typesversion == 3 {
        let mut index = r.u8();
        while index != 0 {
            let _name = read_string(&mut r);
            index = r.u8();
        }
    }

    let proto_count = r.varint() as usize;
    let mut protos: Vec<*mut Proto> = Vec::with_capacity(proto_count);
    let g = (*L).global;
    for i in 0..proto_count {
        let mut proto_size = 0u32;
        if version >= 12 {
            proto_size = r.varint();
        }
        let proto_start = r.offset;
        let p = luaF_newproto(L);
        let m = (*p).hdr.memcat;
        (*p).source = source;
        (*p).bytecodeid = i as i32;
        (*p).funid = if (*g).lastprotoid == 0 {
            0
        } else {
            let id = (*g).lastprotoid;
            (*g).lastprotoid += 1;
            id
        };
        (*p).maxstacksize = r.u8();
        (*p).numparams = r.u8();
        (*p).nups = r.u8();
        (*p).is_vararg = r.u8();
        if version >= 4 {
            (*p).flags = r.u8();
            if typesversion == 1 {
                let typesize = r.varint() as usize;
                if typesize != 0 {
                    let types = &data[r.offset..r.offset + typesize];
                    let headersize = if typesize > 127 { 4 } else { 3 };
                    (*p).typeinfo = luaM_newarray(L, headersize + typesize, m);
                    (*p).sizetypeinfo = (headersize + typesize) as i32;
                    let ti = (*p).typeinfo;
                    if headersize == 4 {
                        *ti = ((typesize & 127) | (1 << 7)) as u8;
                        *ti.add(1) = (typesize >> 7) as u8;
                        *ti.add(2) = 0;
                        *ti.add(3) = 0;
                    } else {
                        *ti = typesize as u8;
                        *ti.add(1) = 0;
                        *ti.add(2) = 0;
                    }
                    std::ptr::copy_nonoverlapping(types.as_ptr(), ti.add(headersize), typesize);
                }
                r.offset += typesize;
            } else if typesversion == 2 || typesversion == 3 {
                let typesize = r.varint() as usize;
                if typesize != 0 {
                    (*p).typeinfo = luaM_newarray(L, typesize, m);
                    (*p).sizetypeinfo = typesize as i32;
                    std::ptr::copy_nonoverlapping(data[r.offset..].as_ptr(), (*p).typeinfo, typesize);
                    r.offset += typesize;
                }
            }
        }

        let sizecode = r.varint() as usize;
        (*p).code = luaM_newarray(L, sizecode, m);
        (*p).sizecode = sizecode as i32;
        for j in 0..sizecode {
            *(*p).code.add(j) = r.u32();
        }
        (*p).codeentry = (*p).code;

        let sizek = r.varint() as usize;
        (*p).k = luaM_newarray(L, sizek, m);
        (*p).sizek = sizek as i32;
        for j in 0..sizek {
            setnilvalue((*p).k.add(j));
        }
        for j in 0..sizek {
            let kj = (*p).k.add(j);
            match r.u8() {
                LBC_CONSTANT_NIL => {}
                LBC_CONSTANT_BOOLEAN => setbvalue(kj, r.u8() != 0),
                LBC_CONSTANT_NUMBER => setnvalue(kj, r.f64()),
                LBC_CONSTANT_VECTOR => {
                    let x = r.f32();
                    let y = r.f32();
                    let z = r.f32();
                    let _w = r.f32();
                    setvvalue(kj, x, y, z);
                }
                LBC_CONSTANT_VECTORD => {
                    let x = r.f64();
                    let y = r.f64();
                    let z = r.f64();
                    let _w = r.f64();
                    setvvalue(kj, x as f32, y as f32, z as f32);
                }
                LBC_CONSTANT_STRING => {
                    let v = read_string(&mut r);
                    setsvalue(kj, v);
                }
                LBC_CONSTANT_IMPORT => {
                    let iid = r.u32();
                    resolve_import_safe(L, (*p).k, iid);
                    setobj(kj, (*L).top.sub(1));
                    (*L).top = (*L).top.sub(1);
                }
                LBC_CONSTANT_TABLE => {
                    let keys = r.varint() as i32;
                    let h = luaH_new(L, 0, keys);
                    for _ in 0..keys {
                        let key = r.varint() as usize;
                        let val = luaH_set(L, h, (*p).k.add(key));
                        setnvalue(val, 0.0);
                    }
                    sethvalue(kj, h);
                }
                LBC_CONSTANT_TABLE_WITH_CONSTANTS => {
                    let keys = r.varint() as i32;
                    let h = luaH_new(L, 0, keys);
                    let mut nil_keys: Vec<usize> = Vec::new();
                    for _ in 0..keys {
                        let key = r.varint() as usize;
                        let val = luaH_set(L, h, (*p).k.add(key));
                        let cidx = r.i32();
                        if cidx >= 0 {
                            let constant = (*p).k.add(cidx as usize);
                            if ttisnil(constant) {
                                nil_keys.push(key);
                            } else {
                                setobj(val, constant);
                                luaC_barriert(L, h, constant);
                                continue;
                            }
                        }
                        setnvalue(val, 0.0);
                    }
                    for key in nil_keys {
                        let val = luaH_set(L, h, (*p).k.add(key));
                        setnilvalue(val);
                    }
                    sethvalue(kj, h);
                }
                LBC_CONSTANT_CLOSURE => {
                    let fid = r.varint() as usize;
                    let cl = luaF_newLclosure(L, (*protos[fid]).nups as i32, envt, protos[fid]);
                    (*cl).preload = ((*cl).nupvalues > 0) as u8;
                    setclvalue(kj, cl);
                }
                LBC_CONSTANT_CLASS_SHAPE => {
                    let cnid = r.varint() as usize;
                    let classname = (*p).k.add(cnid);
                    let num_private_fields = r.u8() as u32;
                    let num_private_methods = r.u8() as u32;
                    let num_properties = r.varint();
                    let num_methods = r.varint();
                    let num_members = num_methods + num_properties;
                    let without_private_methods = num_members - num_private_methods;
                    let offset_to_member: *mut *mut TString = luaM_newarray(L, without_private_methods as usize, (*L).activememcat);
                    for x in 0..without_private_methods as usize {
                        *offset_to_member.add(x) = std::ptr::null_mut();
                    }
                    let members_to_offset = luaH_new(L, 0, (without_private_methods - num_private_fields) as i32);
                    let mut idx = 0u32;
                    while idx < num_properties - num_private_fields {
                        let mid = r.varint() as usize;
                        let name = tsvalue((*p).k.add(mid));
                        *offset_to_member.add(idx as usize) = name;
                        setnvalue(luaH_setstr(L, members_to_offset, name), idx as f64);
                        idx += 1;
                    }
                    idx = num_properties;
                    while idx < without_private_methods {
                        let mid = r.varint() as usize;
                        let name = tsvalue((*p).k.add(mid));
                        *offset_to_member.add(idx as usize) = name;
                        setnvalue(luaH_setstr(L, members_to_offset, name), idx as f64);
                        idx += 1;
                    }
                    (*members_to_offset).readonly = 1;
                    let lco = crate::lclass::luaR_newclass(
                        L,
                        tsvalue(classname),
                        members_to_offset,
                        offset_to_member,
                        num_properties,
                        num_methods,
                        num_private_fields as u8,
                        num_private_methods as u8,
                        envt,
                    );
                    setclassvalue(kj, lco);
                }
                LBC_CONSTANT_INTEGER => {
                    let neg = r.u8() != 0;
                    let magnitude = r.varint64();
                    setlvalue(kj, if neg { magnitude.wrapping_neg() as i64 } else { magnitude as i64 });
                }
                _ => {}
            }
        }

        let sizep = r.varint() as usize;
        (*p).p = luaM_newarray(L, sizep, m);
        (*p).sizep = sizep as i32;
        for j in 0..sizep {
            let fid = r.varint() as usize;
            *(*p).p.add(j) = protos[fid];
        }

        (*p).linedefined = r.varint() as i32;
        (*p).debugname = read_string(&mut r);

        let lineinfo = r.u8();
        if lineinfo != 0 {
            (*p).linegaplog2 = r.u8() as i32;
            let intervals = (((*p).sizecode - 1) >> (*p).linegaplog2) + 1;
            let absoffset = (((*p).sizecode + 3) & !3) as usize;
            let sizelineinfo = absoffset + intervals as usize * 4;
            (*p).lineinfo = luaM_newarray(L, sizelineinfo, m);
            (*p).sizelineinfo = sizelineinfo as i32;
            (*p).abslineinfo = (*p).lineinfo.add(absoffset) as *mut i32;
            let mut lastoffset: u8 = 0;
            for j in 0..(*p).sizecode as usize {
                lastoffset = lastoffset.wrapping_add(r.u8());
                *(*p).lineinfo.add(j) = lastoffset;
            }
            let mut lastline: i32 = 0;
            for j in 0..intervals as usize {
                lastline = lastline.wrapping_add(r.i32());
                *(*p).abslineinfo.add(j) = lastline;
            }
        }

        let debuginfo = r.u8();
        if debuginfo != 0 {
            let sizelocvars = r.varint() as usize;
            (*p).locvars = luaM_newarray(L, sizelocvars, m);
            (*p).sizelocvars = sizelocvars as i32;
            for j in 0..sizelocvars {
                let lv = (*p).locvars.add(j);
                (*lv).varname = read_string(&mut r);
                (*lv).startpc = r.varint() as i32;
                (*lv).endpc = r.varint() as i32;
                (*lv).reg = r.u8();
            }
            let sizeupvalues = r.varint() as usize;
            (*p).upvalues = luaM_newarray(L, sizeupvalues, m);
            (*p).sizeupvalues = sizeupvalues as i32;
            for j in 0..sizeupvalues {
                *(*p).upvalues.add(j) = read_string(&mut r);
            }
        }

        if version >= 11 {
            (*p).feedbackvecsize = r.varint();
            if (*p).feedbackvecsize > 0 {
                (*p).feedbackvec = luaM_newarray(L, (*p).feedbackvecsize as usize, m);
            }
            for j in 0..(*p).feedbackvecsize as usize {
                let slottype = r.u8();
                let slot = (*p).feedbackvec.add(j);
                *slot = FeedbackVectorSlot::default();
                if slottype == LFT_CALLTARGET {
                    (*slot).kind = CALL_TARGET;
                    (*slot).a = r.varint();
                } else if slottype == LFT_CONSTRUCT {
                    (*slot).kind = CONSTRUCT;
                    (*slot).a = r.varint();
                }
            }
        }
        if version >= 12 && ((*p).flags & LPF_INLINABLE) != 0 {
            (*p).cost = r.varint64();
        }
        if version >= 12 {
            r.offset = proto_start + proto_size as usize;
        }
        protos.push(p);
    }

    let mainid = r.varint() as usize;
    let main = protos[mainid];
    luaC_threadbarrier(L);
    let cl = luaF_newLclosure(L, 0, envt, main);
    setclvalue((*L).top, cl);
    incr_top(L);
    0
}

/// `luau_load`: on success pushes the main closure and returns 0; on
/// failure pushes an error message and returns 1.
pub unsafe fn luau_load(L: *mut lua_State, chunkname: &[u8], data: &[u8], env: i32) -> i32 {
    luaC_checkGC(L);
    let g = (*L).global;
    let old = (*g).gcdisabled;
    (*g).gcdisabled = 1;
    let mut result = 0;
    let status = luaD_rawrunprotected(L, &mut || result = loadsafe(L, chunkname, data, env));
    (*g).gcdisabled = old;
    if status == LUA_ERRMEM {
        lua_pushstring(L, LUA_MEMERRMSG);
        return 1;
    }
    result
}
