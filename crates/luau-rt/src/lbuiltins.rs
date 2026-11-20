//! Port of `VM/src/lbuiltins.cpp`: FASTCALL implementations of builtins.
//! Each returns the number of results, or -1 to fall back to the call.

use crate::ldo::expandstacklimit;
use crate::lgc::*;
use crate::lnumutils::*;
use crate::lobject::*;
use crate::lstring::luaS_newlstr;
use crate::ltable::*;
use crate::ltm::*;

pub type luau_FastFunction = unsafe fn(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32;
pub type luau_ProtectedFastFunction = unsafe fn(L: *mut lua_State, ra: StkId, nparams: i32) -> i32;

macro_rules! math1 {
    ($name:ident, $f:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
            if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
                let f: fn(f64) -> f64 = $f;
                setnvalue(res, f(nvalue(arg0)));
                return 1;
            }
            -1
        }
    };
}

macro_rules! math2 {
    ($name:ident, $f:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
                let f: fn(f64, f64) -> f64 = $f;
                setnvalue(res, f(nvalue(arg0), nvalue(args)));
                return 1;
            }
            -1
        }
    };
}

const RPD: f64 = 3.14159265358979323846 / 180.0;

unsafe fn luauF_assert(_L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults == 0 && !l_isfalse(arg0) {
        return 0;
    }
    -1
}

math1!(luauF_abs, f64::abs);
math1!(luauF_acos, crate::libm::acos);
math1!(luauF_asin, crate::libm::asin);
math2!(luauF_atan2, crate::libm::atan2);
math1!(luauF_atan, crate::libm::atan);
math1!(luauF_ceil, crate::lnumutils::luai_ceil);
math1!(luauF_cosh, crate::libm::cosh);
math1!(luauF_cos, crate::libm::cos);
math1!(luauF_deg, |a| a / RPD);
math1!(luauF_exp, crate::libm::exp);
math1!(luauF_floor, crate::lnumutils::luai_floor);
math2!(luauF_fmod, |a, b| a % b);
math1!(luauF_log10, crate::libm::log10);
math2!(luauF_pow, crate::lmathlib::luai_numpow);
math1!(luauF_rad, |a| a * RPD);
math1!(luauF_sinh, crate::libm::sinh);
math1!(luauF_sin, crate::libm::sin);
math1!(luauF_sqrt, f64::sqrt);
math1!(luauF_tanh, crate::libm::tanh);
math1!(luauF_tan, crate::libm::tan);
math1!(luauF_sign, luai_signd);
math1!(luauF_round, crate::lnumutils::luai_round);

unsafe fn luauF_frexp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 2 && ttisnumber(arg0) {
        let (f, e) = crate::lmathlib::frexp(nvalue(arg0));
        setnvalue(res, f);
        setnvalue(res.add(1), e as f64);
        return 2;
    }
    -1
}

unsafe fn luauF_ldexp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        setnvalue(res, crate::lmathlib::ldexp(nvalue(arg0), crate::lvmexecute::d2i_trunc(nvalue(args))));
        return 1;
    }
    -1
}

unsafe fn luauF_log(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        let a1 = nvalue(arg0);
        if nparams == 1 {
            setnvalue(res, crate::libm::log(a1));
            return 1;
        } else if ttisnumber(args) {
            let a2 = nvalue(args);
            let r = if a2 == 2.0 {
                crate::libm::log2(a1)
            } else if a2 == 10.0 {
                crate::libm::log10(a1)
            } else {
                crate::libm::log(a1) / crate::libm::log(a2)
            };
            setnvalue(res, r);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_max(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let a1 = nvalue(arg0);
        let a2 = nvalue(args);
        let mut r = if a2 > a1 { a2 } else { a1 };
        for i in 3..=nparams {
            let a = args.add(i as usize - 2);
            if !ttisnumber(a) {
                return -1;
            }
            let a = nvalue(a);
            r = if a > r { a } else { r };
        }
        setnvalue(res, r);
        return 1;
    }
    -1
}

unsafe fn luauF_min(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let a1 = nvalue(arg0);
        let a2 = nvalue(args);
        let mut r = if a2 < a1 { a2 } else { a1 };
        for i in 3..=nparams {
            let a = args.add(i as usize - 2);
            if !ttisnumber(a) {
                return -1;
            }
            let a = nvalue(a);
            r = if a < r { a } else { r };
        }
        setnvalue(res, r);
        return 1;
    }
    -1
}

unsafe fn luauF_modf(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 2 && ttisnumber(arg0) {
        let (ip, fp) = crate::lmathlib::modf(nvalue(arg0));
        setnvalue(res, ip);
        setnvalue(res.add(1), fp);
        return 2;
    }
    -1
}

#[inline(always)]
fn u32of(d: f64) -> u32 {
    luai_num2unsigned(d)
}

macro_rules! bitn {
    ($name:ident, $init:expr, $op:expr, $fin:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
                let op: fn(u32, u32) -> u32 = $op;
                let mut r = op(u32of(nvalue(arg0)), u32of(nvalue(args)));
                for i in 3..=nparams {
                    let a = args.add(i as usize - 2);
                    if !ttisnumber(a) {
                        return -1;
                    }
                    r = op(r, u32of(nvalue(a)));
                }
                let fin: unsafe fn(StkId, u32) = $fin;
                fin(res, r);
                return 1;
            }
            -1
        }
    };
}

unsafe fn setnum_u32(res: StkId, r: u32) {
    setnvalue(res, r as f64);
}
unsafe fn setbool_u32(res: StkId, r: u32) {
    setbvalue(res, r != 0);
}

bitn!(luauF_band, 0, |a, b| a & b, setnum_u32);
bitn!(luauF_bor, 0, |a, b| a | b, setnum_u32);
bitn!(luauF_bxor, 0, |a, b| a ^ b, setnum_u32);
bitn!(luauF_btest, 0, |a, b| a & b, setbool_u32);

unsafe fn luauF_arshift(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let u = u32of(nvalue(arg0));
        let s = crate::lvmexecute::d2i_trunc(nvalue(args));
        if (s as u32) < 32 {
            setnvalue(res, ((u as i32) >> s) as u32 as f64);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_bnot(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        setnvalue(res, (!u32of(nvalue(arg0))) as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_extract(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let n = u32of(nvalue(arg0));
        let f = crate::lvmexecute::d2i_trunc(nvalue(args));
        if nparams == 2 {
            if (f as u32) < 32 {
                setnvalue(res, ((n >> f) & 1) as f64);
                return 1;
            }
        } else if ttisnumber(args.add(1)) {
            let w = nvalue(args.add(1)) as i32;
            if f >= 0 && w > 0 && f + w <= 32 {
                let m = !(0xfffffffeu32 << (w - 1));
                setnvalue(res, ((n >> f) & m) as f64);
                return 1;
            }
        }
    }
    -1
}

unsafe fn luauF_lrotate(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let u = u32of(nvalue(arg0));
        let s = crate::lvmexecute::d2i_trunc(nvalue(args));
        setnvalue(res, u.rotate_left((s & 31) as u32) as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_rrotate(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let u = u32of(nvalue(arg0));
        let s = crate::lvmexecute::d2i_trunc(nvalue(args));
        setnvalue(res, u.rotate_right((s & 31) as u32) as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_lshift(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let u = u32of(nvalue(arg0));
        let s = crate::lvmexecute::d2i_trunc(nvalue(args));
        if (s as u32) < 32 {
            setnvalue(res, (u << s) as f64);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_rshift(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let u = u32of(nvalue(arg0));
        let s = crate::lvmexecute::d2i_trunc(nvalue(args));
        if (s as u32) < 32 {
            setnvalue(res, (u >> s) as f64);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_replace(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
        let n = u32of(nvalue(arg0));
        let v = u32of(nvalue(args));
        let f = nvalue(args.add(1)) as i32;
        if nparams == 3 {
            if (f as u32) < 32 {
                let m = 1u32;
                setnvalue(res, ((n & !(m << f)) | ((v & m) << f)) as f64);
                return 1;
            }
        } else if ttisnumber(args.add(2)) {
            let w = nvalue(args.add(2)) as i32;
            if f >= 0 && w > 0 && f + w <= 32 {
                let m = !(0xfffffffeu32 << (w - 1));
                setnvalue(res, ((n & !(m << f)) | ((v & m) << f)) as f64);
                return 1;
            }
        }
    }
    -1
}

unsafe fn luauF_type(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 {
        setsvalue(res, (*(*L).global).ttname[(*arg0).tt as usize]);
        return 1;
    }
    -1
}

unsafe fn luauF_byte(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && ttisstring(arg0) && ttisnumber(args) {
        let ts = tsvalue(arg0);
        let i = crate::lvmexecute::d2i_trunc(nvalue(args));
        let j = if nparams >= 3 {
            if ttisnumber(args.add(1)) {
                nvalue(args.add(1)) as i32
            } else {
                0
            }
        } else {
            i
        };
        if i >= 1 && j >= i && j <= (*ts).len as i32 {
            let c = j - i + 1;
            let s = getstr(ts);
            if c == if nresults < 0 { 1 } else { nresults } {
                for kk in 0..c {
                    setnvalue(res.add(kk as usize), *s.add((i + kk - 1) as usize) as f64);
                }
                return c;
            }
        }
    }
    -1
}

unsafe fn luauF_char(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    let mut buffer = [0u8; 8];
    if nparams < 8 && nresults <= 1 {
        if luaC_needsGC(L) {
            return -1;
        }
        if nparams >= 1 {
            if !ttisnumber(arg0) {
                return -1;
            }
            let ch = crate::lvmexecute::d2i_trunc(nvalue(arg0));
            if (ch as u8) as i32 != ch {
                return -1;
            }
            buffer[0] = ch as u8;
        }
        for i in 2..=nparams {
            let a = args.add(i as usize - 2);
            if !ttisnumber(a) {
                return -1;
            }
            let ch = crate::lvmexecute::d2i_trunc(nvalue(a));
            if (ch as u8) as i32 != ch {
                return -1;
            }
            buffer[i as usize - 1] = ch as u8;
        }
        setsvalue(res, luaS_newlstr(L, &buffer[..nparams.max(0) as usize]));
        return 1;
    }
    -1
}

unsafe fn luauF_len(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisstring(arg0) {
        setnvalue(res, (*tsvalue(arg0)).len as i32 as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_typeof(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 {
        setsvalue(res, luaT_objtypenamestr(L, arg0));
        return 1;
    }
    -1
}

unsafe fn luauF_sub(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisstring(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
        let ts = tsvalue(arg0);
        let i = crate::lvmexecute::d2i_trunc(nvalue(args));
        let j = nvalue(args.add(1)) as i32;
        if luaC_needsGC(L) {
            return -1;
        }
        if i >= 1 && j >= i && ((j - 1) as u32) < (*ts).len {
            setsvalue(res, luaS_newlstr(L, &sbytes(ts)[(i - 1) as usize..j as usize]));
            return 1;
        }
    }
    -1
}

unsafe fn luauF_clamp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
        let v = nvalue(arg0);
        let min = nvalue(args);
        let max = nvalue(args.add(1));
        if min <= max {
            let r = if v < min { min } else { v };
            let r = if r > max { max } else { r };
            setnvalue(res, r);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_rawequal(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 {
        setbvalue(res, luaO_rawequalObj(arg0, args));
        return 1;
    }
    -1
}

unsafe fn luauF_rawget(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttistable(arg0) {
        setobj(res, luaH_get(hvalue(arg0), args));
        return 1;
    }
    -1
}

unsafe fn luauF_rawset(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttistable(arg0) {
        let key = args;
        if ttisnil(key) {
            return -1;
        } else if ttisnumber(key) && nvalue(key).is_nan() {
            return -1;
        } else if ttisvector(key) {
            let v = vvalue(key);
            if (*v).is_nan() || (*v.add(1)).is_nan() || f32::from_bits(*(v.add(2) as *const u32)).is_nan() {
                return -1;
            }
        }
        let t = hvalue(arg0);
        if (*t).readonly != 0 {
            return -1;
        }
        setobj(res, arg0);
        setobj(luaH_set(L, t, args), args.add(1));
        luaC_barriert(L, t, args.add(1));
        return 1;
    }
    -1
}

unsafe fn luauF_tinsert(L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams == 2 && nresults <= 0 && ttistable(arg0) {
        let t = hvalue(arg0);
        if (*t).readonly != 0 {
            return -1;
        }
        let pos = luaH_getn(t) + 1;
        setobj(luaH_setnum(L, t, pos), args);
        luaC_barriert(L, t, args);
        return 0;
    }
    -1
}

unsafe fn luauF_tunpack(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults < 0 && ttistable(arg0) {
        let t = hvalue(arg0);
        let mut n = -1;
        if nparams == 1 {
            n = luaH_getn(t);
        } else if nparams == 3 && ttisnumber(args) && ttisnumber(args.add(1)) && nvalue(args) == 1.0 {
            n = nvalue(args.add(1)) as i32;
        }
        if n >= 0 && n <= (*t).sizearray && (*L).stack_last.offset_from(res) as i32 >= n && n + nparams <= LUAI_MAXCSTACK {
            let array = (*t).array;
            for i in 0..n as usize {
                setobj(res.add(i), array.add(i));
            }
            expandstacklimit(L, res.add(n as usize));
            return n;
        }
    }
    -1
}

unsafe fn luauF_vector(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) {
        let x = nvalue(arg0) as f32;
        let y = nvalue(args) as f32;
        let mut z = 0.0f32;
        if nparams >= 3 {
            if !ttisnumber(args.add(1)) {
                return -1;
            }
            z = nvalue(args.add(1)) as f32;
        }
        setvvalue(res, x, y, z);
        return 1;
    }
    -1
}

unsafe fn luauF_countlz(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        setnvalue(res, u32of(nvalue(arg0)).leading_zeros() as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_countrz(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        setnvalue(res, u32of(nvalue(arg0)).trailing_zeros() as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_select(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams == 1 && nresults == 1 {
        let n = (*L).base.offset_from((*(*L).ci).func) as i32 - (*(*(*L).ci).p).numparams as i32 - 1;
        if ttisnumber(arg0) {
            let i = crate::lvmexecute::d2i_trunc(nvalue(arg0));
            if ((i - 1) as u32) < n as u32 {
                setobj(res, (*L).base.sub(n as usize).add((i - 1) as usize));
                return 1;
            }
        } else if ttisstring(arg0) && *getstr(tsvalue(arg0)) == b'#' {
            setnvalue(res, n as f64);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_rawlen(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 {
        if ttistable(arg0) {
            setnvalue(res, luaH_getn(hvalue(arg0)) as f64);
            return 1;
        } else if ttisstring(arg0) {
            setnvalue(res, (*tsvalue(arg0)).len as f64);
            return 1;
        }
    }
    -1
}

unsafe fn luauF_extractk(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisnumber(arg0) {
        let n = u32of(nvalue(arg0));
        let fw = crate::lvmexecute::d2i_trunc(nvalue(args));
        let f = fw & 31;
        let w1 = fw >> 5;
        let m = !(0xfffffffeu32 << w1);
        setnvalue(res, ((n >> f) & m) as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_getmetatable(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 {
        let mt = if ttistable(arg0) {
            (*hvalue(arg0)).metatable
        } else if ttisuserdata(arg0) {
            (*uvalue(arg0)).metatable
        } else {
            (*(*L).global).mt[(*arg0).tt as usize]
        };
        let mtv = if mt.is_null() { luaO_nilobject() } else { luaH_getstr(mt, (*(*L).global).tmname[TM_METATABLE as usize]) };
        if !ttisnil(mtv) {
            setobj(res, mtv);
            return 1;
        }
        if !mt.is_null() {
            sethvalue(res, mt);
        } else {
            setnilvalue(res);
        }
        return 1;
    }
    -1
}

unsafe fn luauF_setmetatable(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttistable(arg0) && ttistable(args) {
        let t = hvalue(arg0);
        if (*t).readonly != 0 || !(*t).metatable.is_null() {
            return -1;
        }
        let mt = hvalue(args);
        (*t).metatable = mt;
        luaC_objbarrier(L, t as *mut GCObject, mt as *mut GCObject);
        sethvalue(res, t);
        return 1;
    }
    -1
}

unsafe fn luauF_tonumber(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams == 1 && nresults <= 1 {
        if ttisnumber(arg0) {
            setnvalue(res, nvalue(arg0));
        } else if let Some(num) = if ttisstring(arg0) { luaO_str2d(sbytes(tsvalue(arg0))) } else { None } {
            setnvalue(res, num);
        } else {
            setnilvalue(res);
        }
        return 1;
    }
    -1
}

unsafe fn luauF_tostring(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 {
        match (*arg0).tt {
            LUA_TNIL => {
                setsvalue(res, (*(*L).global).ttname[LUA_TNIL as usize]);
                return 1;
            }
            LUA_TBOOLEAN => {
                setsvalue(res, luaS_newlstr(L, if bvalue(arg0) != 0 { b"true" } else { b"false" }));
                return 1;
            }
            LUA_TNUMBER => {
                if luaC_needsGC(L) {
                    return -1;
                }
                let mut nb = crate::lnumutils::NumBuf::new();
                let s = crate::lnumutils::luai_num2buf(&mut nb, nvalue(arg0));
                setsvalue(res, luaS_newlstr(L, s));
                return 1;
            }
            LUA_TSTRING => {
                setsvalue(res, tsvalue(arg0));
                return 1;
            }
            LUA_TINTEGER => {
                if luaC_needsGC(L) {
                    return -1;
                }
                let mut nb = crate::lnumutils::NumBuf::new();
                let s = crate::lnumutils::luai_int2buf(&mut nb, lvalue(arg0));
                setsvalue(res, luaS_newlstr(L, s));
                return 1;
            }
            _ => {}
        }
    }
    -1
}

unsafe fn luauF_byteswap(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        setnvalue(res, u32of(nvalue(arg0)).swap_bytes() as f64);
        return 1;
    }
    -1
}

#[inline(always)]
fn checkoutofbounds(offset: i32, len: u32, accesssize: u64) -> bool {
    (offset as u32 as u64) + (accesssize - 1) >= len as u64
}

macro_rules! buf_readint {
    ($name:ident, $t:ty) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisbuffer(arg0) && ttisnumber(args) {
                let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
                let b = bufvalue(arg0);
                if checkoutofbounds(offset, (*b).len, std::mem::size_of::<$t>() as u64) {
                    return -1;
                }
                let v = std::ptr::read_unaligned(Buffer::data(b).add(offset as u32 as usize) as *const $t);
                setnvalue(res, v as f64);
                return 1;
            }
            -1
        }
    };
}

macro_rules! buf_writeint {
    ($name:ident, $t:ty) => {
        unsafe fn $name(_L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 3 && nresults <= 0 && ttisbuffer(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
                let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
                let b = bufvalue(arg0);
                if checkoutofbounds(offset, (*b).len, std::mem::size_of::<$t>() as u64) {
                    return -1;
                }
                let value = u32of(nvalue(args.add(1)));
                std::ptr::write_unaligned(Buffer::data(b).add(offset as u32 as usize) as *mut $t, value as $t);
                return 0;
            }
            -1
        }
    };
}

macro_rules! buf_readfp {
    ($name:ident, $t:ty) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisbuffer(arg0) && ttisnumber(args) {
                let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
                let b = bufvalue(arg0);
                if checkoutofbounds(offset, (*b).len, std::mem::size_of::<$t>() as u64) {
                    return -1;
                }
                let v = std::ptr::read_unaligned(Buffer::data(b).add(offset as u32 as usize) as *const $t);
                setnvalue(res, v as f64);
                return 1;
            }
            -1
        }
    };
}

macro_rules! buf_writefp {
    ($name:ident, $t:ty) => {
        unsafe fn $name(_L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 3 && nresults <= 0 && ttisbuffer(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
                let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
                let b = bufvalue(arg0);
                if checkoutofbounds(offset, (*b).len, std::mem::size_of::<$t>() as u64) {
                    return -1;
                }
                std::ptr::write_unaligned(Buffer::data(b).add(offset as u32 as usize) as *mut $t, nvalue(args.add(1)) as $t);
                return 0;
            }
            -1
        }
    };
}

buf_readint!(luauF_readi8, i8);
buf_readint!(luauF_readu8, u8);
buf_writeint!(luauF_writeu8, u8);
buf_readint!(luauF_readi16, i16);
buf_readint!(luauF_readu16, u16);
buf_writeint!(luauF_writeu16, u16);
buf_readint!(luauF_readi32, i32);
buf_readint!(luauF_readu32, u32);
buf_writeint!(luauF_writeu32, u32);
buf_readfp!(luauF_readf32, f32);
buf_writefp!(luauF_writef32, f32);
buf_readfp!(luauF_readf64, f64);
buf_writefp!(luauF_writef64, f64);

#[inline(always)]
unsafe fn v3(o: *const TValue) -> [f32; 3] {
    let v = vvalue(o);
    [*v, *v.add(1), f32::from_bits(*(v.add(2) as *const u32))]
}

macro_rules! vec1 {
    ($name:ident, $f:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
            if nparams >= 1 && nresults <= 1 && ttisvector(arg0) {
                let f: fn(f32) -> f32 = $f;
                let v = v3(arg0);
                setvvalue(res, f(v[0]), f(v[1]), f(v[2]));
                return 1;
            }
            -1
        }
    };
}

vec1!(luauF_vectorfloor, f32::floor);
vec1!(luauF_vectorceil, f32::ceil);
vec1!(luauF_vectorabs, f32::abs);
vec1!(luauF_vectorsign, luai_signf);

unsafe fn luauF_vectormagnitude(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisvector(arg0) {
        let v = v3(arg0);
        setnvalue(res, (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt() as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_vectornormalize(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisvector(arg0) {
        let v = v3(arg0);
        let inv = 1.0f32 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        setvvalue(res, v[0] * inv, v[1] * inv, v[2] * inv);
        return 1;
    }
    -1
}

unsafe fn luauF_vectorcross(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisvector(arg0) && ttisvector(args) {
        let a = v3(arg0);
        let b = v3(args);
        setvvalue(res, a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]);
        return 1;
    }
    -1
}

unsafe fn luauF_vectordot(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisvector(arg0) && ttisvector(args) {
        let a = v3(arg0);
        let b = v3(args);
        setnvalue(res, (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) as f64);
        return 1;
    }
    -1
}

unsafe fn luauF_vectorclamp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisvector(arg0) && ttisvector(args) && ttisvector(args.add(1)) {
        let v = v3(arg0);
        let mn = v3(args);
        let mx = v3(args.add(1));
        if mn[0] <= mx[0] && mn[1] <= mx[1] && mn[2] <= mx[2] {
            setvvalue(res, luai_clampf(v[0], mn[0], mx[0]), luai_clampf(v[1], mn[1], mx[1]), luai_clampf(v[2], mn[2], mx[2]));
            return 1;
        }
    }
    -1
}

macro_rules! vecminmax {
    ($name:ident, $cmp:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisvector(arg0) && ttisvector(args) {
                let pick: fn(f32, f32) -> bool = $cmp;
                let a = v3(arg0);
                let b = v3(args);
                let mut r = [0f32; 3];
                for i in 0..3 {
                    r[i] = if pick(b[i], a[i]) { b[i] } else { a[i] };
                }
                for i in 3..=nparams {
                    let c = args.add(i as usize - 2);
                    if !ttisvector(c) {
                        return -1;
                    }
                    let c = v3(c);
                    for j in 0..3 {
                        r[j] = if pick(c[j], r[j]) { c[j] } else { r[j] };
                    }
                }
                setvvalue(res, r[0], r[1], r[2]);
                return 1;
            }
            -1
        }
    };
}

vecminmax!(luauF_vectormin, |x, y| x < y);
vecminmax!(luauF_vectormax, |x, y| x > y);

unsafe fn luauF_vectorlerp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisvector(arg0) && ttisvector(args) && ttisnumber(args.add(1)) {
        let a = v3(arg0);
        let b = v3(args);
        let t = nvalue(args.add(1)) as f32;
        setvvalue(res, luai_lerpf(a[0], b[0], t), luai_lerpf(a[1], b[1], t), luai_lerpf(a[2], b[2], t));
        return 1;
    }
    -1
}

unsafe fn luauF_lerp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisnumber(arg0) && ttisnumber(args) && ttisnumber(args.add(1)) {
        let a = nvalue(arg0);
        let b = nvalue(args);
        let t = nvalue(args.add(1));
        setnvalue(res, if t == 1.0 { b } else { a + (b - a) * t });
        return 1;
    }
    -1
}

macro_rules! numpred {
    ($name:ident, $f:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
            if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
                let f: fn(f64) -> bool = $f;
                setbvalue(res, f(nvalue(arg0)));
                return 1;
            }
            -1
        }
    };
}

numpred!(luauF_isnan, f64::is_nan);
numpred!(luauF_isinf, f64::is_infinite);
numpred!(luauF_isfinite, f64::is_finite);

// ---------------------------------------------------------------------------
// integer library
// ---------------------------------------------------------------------------

unsafe fn luauF_integertonumber(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setnvalue(res, lvalue(arg0) as f64);
        return 1;
    }
    -1
}

macro_rules! int2 {
    ($name:ident, |$a:ident, $b:ident| $body:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisinteger(arg0) && ttisinteger(args) {
                let $a = lvalue(arg0);
                let $b = lvalue(args);
                let r: Option<TValue> = $body;
                match r {
                    Some(v) => {
                        *res = v;
                        return 1;
                    }
                    None => return -1,
                }
            }
            -1
        }
    };
}

#[inline(always)]
fn ival(x: i64) -> Option<TValue> {
    let mut t = TValue::NIL;
    unsafe { setlvalue(&mut t, x) };
    Some(t)
}
#[inline(always)]
fn bval(x: bool) -> Option<TValue> {
    let mut t = TValue::NIL;
    unsafe { setbvalue(&mut t, x) };
    Some(t)
}

int2!(luauF_integeradd, |a, b| ival(a.wrapping_add(b)));
int2!(luauF_integersub, |a, b| ival(a.wrapping_sub(b)));
int2!(luauF_integermul, |a, b| ival(a.wrapping_mul(b)));
int2!(luauF_integerdiv, |a, b| if b == 0 || (a == i64::MIN && b == -1) { None } else { ival(a / b) });
int2!(luauF_integerudiv, |a, b| if b == 0 { None } else { ival(((a as u64) / (b as u64)) as i64) });
int2!(luauF_integerurem, |a, b| if b == 0 { None } else { ival(((a as u64) % (b as u64)) as i64) });
int2!(luauF_integerrem, |a, b| if b == 0 {
    None
} else {
    ival(if a == i64::MIN && b == -1 { 0 } else { a % b })
});
int2!(luauF_integermod, |a, b| if b == 0 {
    None
} else if a == i64::MIN && b == -1 {
    ival(0)
} else {
    let mut r = a % b;
    if r != 0 && ((a < 0) != (b < 0)) {
        r += b;
    }
    ival(r)
});
int2!(luauF_integeridiv, |a, b| if b == 0 || (a == i64::MIN && b == -1) {
    None
} else {
    let mut r = a / b;
    if ((a ^ b) < 0) && (a % b != 0) {
        r -= 1;
    }
    ival(r)
});
int2!(luauF_integerlt, |a, b| bval(a < b));
int2!(luauF_integerle, |a, b| bval(a <= b));
int2!(luauF_integergt, |a, b| bval(a > b));
int2!(luauF_integerge, |a, b| bval(a >= b));
int2!(luauF_integerult, |a, b| bval((a as u64) < (b as u64)));
int2!(luauF_integerule, |a, b| bval((a as u64) <= (b as u64)));
int2!(luauF_integerugt, |a, b| bval((a as u64) > (b as u64)));
int2!(luauF_integeruge, |a, b| bval((a as u64) >= (b as u64)));
int2!(luauF_integerlrotate, |a, b| ival((a as u64).rotate_left(((b as u64) % 64) as u32) as i64));
int2!(luauF_integerrrotate, |a, b| ival((a as u64).rotate_right(((b as u64) % 64) as u32) as i64));
int2!(luauF_integerlshift, |a, b| ival(if (-63..=63).contains(&b) {
    if b < 0 {
        ((a as u64) >> (-b)) as i64
    } else {
        ((a as u64) << b) as i64
    }
} else {
    0
}));
int2!(luauF_integerrshift, |a, b| ival(if (-63..=63).contains(&b) {
    if b < 0 {
        ((a as u64) << (-b)) as i64
    } else {
        ((a as u64) >> b) as i64
    }
} else {
    0
}));
int2!(luauF_integerarshift, |a, b| ival(if (-63..=63).contains(&b) {
    if b < 0 {
        ((a as u64) << (-b)) as i64
    } else {
        a >> b
    }
} else if b < -63 {
    0
} else if a < 0 {
    -1
} else {
    0
}));

unsafe fn luauF_integerneg(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setlvalue(res, lvalue(arg0).wrapping_neg());
        return 1;
    }
    -1
}

unsafe fn luauF_integerbnot(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setlvalue(res, !lvalue(arg0));
        return 1;
    }
    -1
}

unsafe fn luauF_integerbswap(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setlvalue(res, lvalue(arg0).swap_bytes());
        return 1;
    }
    -1
}

unsafe fn luauF_integercountlz(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setlvalue(res, (lvalue(arg0) as u64).leading_zeros() as i64);
        return 1;
    }
    -1
}

unsafe fn luauF_integercountrz(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
        setlvalue(res, (lvalue(arg0) as u64).trailing_zeros() as i64);
        return 1;
    }
    -1
}

macro_rules! intfold {
    ($name:ident, $op:expr, $bool:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 1 && nresults <= 1 && ttisinteger(arg0) {
                let op: fn(u64, u64) -> u64 = $op;
                let mut r = lvalue(arg0) as u64;
                for i in 2..=nparams {
                    let a = args.add(i as usize - 2);
                    if !ttisinteger(a) {
                        return -1;
                    }
                    r = op(r, lvalue(a) as u64);
                }
                if $bool {
                    setbvalue(res, r != 0);
                } else {
                    setlvalue(res, r as i64);
                }
                return 1;
            }
            -1
        }
    };
}

intfold!(luauF_integerband, |a, b| a & b, false);
intfold!(luauF_integerbor, |a, b| a | b, false);
intfold!(luauF_integerbxor, |a, b| a ^ b, false);
intfold!(luauF_integerbtest, |a, b| a & b, true);

macro_rules! intminmax {
    ($name:ident, $pick:expr) => {
        unsafe fn $name(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
            if nparams >= 2 && nresults <= 1 && ttisinteger(arg0) && ttisinteger(args) {
                let pick: fn(i64, i64) -> bool = $pick;
                let a1 = lvalue(arg0);
                let a2 = lvalue(args);
                let mut r = if pick(a2, a1) { a2 } else { a1 };
                for i in 3..=nparams {
                    let a = args.add(i as usize - 2);
                    if !ttisinteger(a) {
                        return -1;
                    }
                    let a = lvalue(a);
                    r = if pick(a, r) { a } else { r };
                }
                setlvalue(res, r);
                return 1;
            }
            -1
        }
    };
}

intminmax!(luauF_integermin, |x, y| x < y);
intminmax!(luauF_integermax, |x, y| x > y);

unsafe fn luauF_integerextract(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && !ttisinteger(args.add(1)) {
        return -1;
    }
    if nparams >= 2 && nresults <= 1 && ttisinteger(arg0) && ttisinteger(args) {
        let n = lvalue(arg0);
        let f = lvalue(args);
        let w = if nparams >= 3 { lvalue(args.add(1)) } else { 1 };
        if !(0..=63).contains(&f) || !(1..=64).contains(&w) || f + w > 64 {
            return -1;
        }
        setlvalue(res, (((n as u64) >> f) & (u64::MAX >> (64 - w))) as i64);
        return 1;
    }
    -1
}

unsafe fn luauF_integerclamp(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 1 && ttisinteger(arg0) && ttisinteger(args) && ttisinteger(args.add(1)) {
        let a = lvalue(arg0);
        let rmin = lvalue(args);
        let rmax = lvalue(args.add(1));
        if rmin > rmax {
            return -1;
        }
        setlvalue(res, if a < rmin { rmin } else if a > rmax { rmax } else { a });
        return 1;
    }
    -1
}

unsafe fn luauF_integercreate(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        let a1 = nvalue(arg0);
        if (-9223372036854775808.0..9223372036854775808.0).contains(&a1) {
            let x = a1 as i64;
            if x as f64 == a1 {
                setlvalue(res, x);
                return 1;
            }
        }
        setnilvalue(res);
        return 1;
    }
    -1
}

unsafe fn luauF_bufferreadlong(_L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 2 && nresults <= 1 && ttisbuffer(arg0) && ttisnumber(args) {
        let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
        let b = bufvalue(arg0);
        if checkoutofbounds(offset, (*b).len, 8) {
            return -1;
        }
        setlvalue(res, std::ptr::read_unaligned(Buffer::data(b).add(offset as u32 as usize) as *const i64));
        return 1;
    }
    -1
}

unsafe fn luauF_bufferwritelong(_L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 3 && nresults <= 0 && ttisbuffer(arg0) && ttisnumber(args) && ttisinteger(args.add(1)) {
        let offset = crate::lvmexecute::d2i_trunc(nvalue(args));
        let b = bufvalue(arg0);
        if checkoutofbounds(offset, (*b).len, 8) {
            return -1;
        }
        std::ptr::write_unaligned(Buffer::data(b).add(offset as u32 as usize) as *mut i64, lvalue(args.add(1)));
        return 0;
    }
    -1
}

unsafe fn luauF_tablecreate(L: *mut lua_State, res: StkId, arg0: *mut TValue, nresults: i32, args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 1 && ttisnumber(arg0) {
        let size = crate::lvmexecute::d2i_trunc(nvalue(arg0));
        if size < 0 || luaC_needsGC(L) {
            return -1;
        }
        let t = luaH_new(L, size, 0);
        if nparams >= 2 {
            for i in 0..size as usize {
                setobj((*t).array.add(i), args);
            }
        }
        sethvalue(res, t);
        return 1;
    }
    -1
}

unsafe fn luauF_tableclear(_L: *mut lua_State, _res: StkId, arg0: *mut TValue, nresults: i32, _args: StkId, nparams: i32) -> i32 {
    if nparams >= 1 && nresults <= 0 && ttistable(arg0) {
        let t = hvalue(arg0);
        if (*t).readonly != 0 {
            return -1;
        }
        luaH_clear(t);
        return 0;
    }
    -1
}

unsafe fn luauF_missing(_L: *mut lua_State, _res: StkId, _arg0: *mut TValue, _nresults: i32, _args: StkId, _nparams: i32) -> i32 {
    -1
}

pub static luauF_table: [luau_FastFunction; 256] = {
    let mut t: [luau_FastFunction; 256] = [luauF_missing; 256];
    let list: &[luau_FastFunction] = &[
        luauF_missing,
        luauF_assert,
        luauF_abs,
        luauF_acos,
        luauF_asin,
        luauF_atan2,
        luauF_atan,
        luauF_ceil,
        luauF_cosh,
        luauF_cos,
        luauF_deg,
        luauF_exp,
        luauF_floor,
        luauF_fmod,
        luauF_frexp,
        luauF_ldexp,
        luauF_log10,
        luauF_log,
        luauF_max,
        luauF_min,
        luauF_modf,
        luauF_pow,
        luauF_rad,
        luauF_sinh,
        luauF_sin,
        luauF_sqrt,
        luauF_tanh,
        luauF_tan,
        luauF_arshift,
        luauF_band,
        luauF_bnot,
        luauF_bor,
        luauF_bxor,
        luauF_btest,
        luauF_extract,
        luauF_lrotate,
        luauF_lshift,
        luauF_replace,
        luauF_rrotate,
        luauF_rshift,
        luauF_type,
        luauF_byte,
        luauF_char,
        luauF_len,
        luauF_typeof,
        luauF_sub,
        luauF_clamp,
        luauF_sign,
        luauF_round,
        luauF_rawset,
        luauF_rawget,
        luauF_rawequal,
        luauF_tinsert,
        luauF_tunpack,
        luauF_vector,
        luauF_countlz,
        luauF_countrz,
        luauF_select,
        luauF_rawlen,
        luauF_extractk,
        luauF_getmetatable,
        luauF_setmetatable,
        luauF_tonumber,
        luauF_tostring,
        luauF_byteswap,
        luauF_readi8,
        luauF_readu8,
        luauF_writeu8,
        luauF_readi16,
        luauF_readu16,
        luauF_writeu16,
        luauF_readi32,
        luauF_readu32,
        luauF_writeu32,
        luauF_readf32,
        luauF_writef32,
        luauF_readf64,
        luauF_writef64,
        luauF_vectormagnitude,
        luauF_vectornormalize,
        luauF_vectorcross,
        luauF_vectordot,
        luauF_vectorfloor,
        luauF_vectorceil,
        luauF_vectorabs,
        luauF_vectorsign,
        luauF_vectorclamp,
        luauF_vectormin,
        luauF_vectormax,
        luauF_lerp,
        luauF_vectorlerp,
        luauF_isnan,
        luauF_isinf,
        luauF_isfinite,
        luauF_integercreate,
        luauF_integertonumber,
        luauF_integerneg,
        luauF_integeradd,
        luauF_integersub,
        luauF_integermul,
        luauF_integerdiv,
        luauF_integermin,
        luauF_integermax,
        luauF_integerrem,
        luauF_integeridiv,
        luauF_integerudiv,
        luauF_integerurem,
        luauF_integermod,
        luauF_integerclamp,
        luauF_integerband,
        luauF_integerbor,
        luauF_integerbnot,
        luauF_integerbxor,
        luauF_integerlt,
        luauF_integerle,
        luauF_integerult,
        luauF_integerule,
        luauF_integergt,
        luauF_integerge,
        luauF_integerugt,
        luauF_integeruge,
        luauF_integerlshift,
        luauF_integerrshift,
        luauF_integerarshift,
        luauF_integerlrotate,
        luauF_integerrrotate,
        luauF_integerextract,
        luauF_integerbtest,
        luauF_integercountrz,
        luauF_integercountlz,
        luauF_integerbswap,
        luauF_bufferreadlong,
        luauF_bufferwritelong,
        luauF_tablecreate,
        luauF_tableclear,
        luauF_missing,
        luauF_missing,
        luauF_missing,
        luauF_missing,
        luauF_missing,
        luauF_missing,
        luauF_missing,
        luauF_missing,
    ];
    let mut i = 0;
    while i < list.len() {
        t[i] = list[i];
        i += 1;
    }
    t
};

unsafe fn luauPF_pcall(L: *mut lua_State, ra: StkId, nparams: i32) -> i32 {
    let g = (*L).global;
    if nparams < 1 || !ttisfunction(ra.add(1)) || (*g).builtinPcall.is_null() {
        return -1;
    }
    setclvalue(ra, (*g).builtinPcall);
    0
}

unsafe fn luauPF_xpcall(L: *mut lua_State, ra: StkId, nparams: i32) -> i32 {
    let g = (*L).global;
    if nparams < 2 || !ttisfunction(ra.add(1)) || !ttisfunction(ra.add(2)) || (*g).builtinXpcall.is_null() {
        return -1;
    }
    setclvalue(ra, (*g).builtinXpcall);
    let tmp = *ra.add(1);
    setobj(ra.add(1), ra.add(2));
    *ra.add(2) = tmp;
    1
}

pub static luauPF_table: [luau_ProtectedFastFunction; 2] = [luauPF_pcall, luauPF_xpcall];
