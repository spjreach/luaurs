//! Numeric helpers: `lnumutils.h`, `lnumprint.cpp` (Schubfach shortest
//! round-trip printing) and C library string-to-number conversions with
//! `strtod`/`strtol` semantics.

#![allow(clippy::unreadable_literal)]

pub const LUAI_MAXNUM2STR: usize = 48;
pub const LUAI_MAXINT2STR: usize = 30;

/// C `trunc` without the libm call (MinGW has no inline trunc for SSE2).
#[inline(always)]
pub fn luai_trunc(x: f64) -> f64 {
    if x.abs() < 4503599627370496.0 {
        (d2l_trunc(x) as f64).copysign(x)
    } else {
        x
    }
}

/// C `floorf` without the libm call.
#[inline(always)]
pub fn luai_floorf(x: f32) -> f32 {
    if x.abs() < 8388608.0 {
        #[cfg(target_arch = "x86_64")]
        let t = unsafe { core::arch::x86_64::_mm_cvttss_si32(core::arch::x86_64::_mm_set_ss(x)) } as f32;
        #[cfg(not(target_arch = "x86_64"))]
        let t = x as i32 as f32;
        let f = if t > x { t - 1.0 } else { t };
        if f == 0.0 { 0.0f32.copysign(x) } else { f }
    } else {
        x
    }
}

/// C `round` (half away from zero). Not `f64::round`: on MinGW that lowers
/// to a libm routine computing `floor(x + 0.5)`, which is wrong for
/// 0.49999999999999994.
#[inline(always)]
pub fn luai_round(x: f64) -> f64 {
    let t = luai_trunc(x);
    if (x - t).abs() >= 0.5 {
        t + x.signum()
    } else {
        t
    }
}

/// `ceil` without a libm call.
#[inline(always)]
pub fn luai_ceil(x: f64) -> f64 {
    -luai_floor(-x)
}

pub fn luai_nummod(a: f64, b: f64) -> f64 {
    a - luai_floor(a / b) * b
}

/// Exact `floor` without a libm call (MinGW's `floor` is an out-of-line
/// routine): values of 2^52 and above are already integral.
#[inline(always)]
pub fn luai_floor(x: f64) -> f64 {
    if x.abs() < 4503599627370496.0 {
        let t = d2l_trunc(x) as f64;
        let f = if t > x { t - 1.0 } else { t };
        // keep the sign of zero results (floor(-0.0) == -0.0, floor(-0.5) == -1)
        if f == 0.0 { 0.0f64.copysign(x) } else { f }
    } else {
        x
    }
}
/// `floor` using SSE4.1 `roundsd` when the CPU has it (the reference VM
/// picks `luauF_floor_sse41` the same way), else [`luai_floor`].
#[inline(always)]
pub fn luai_floor_fast(x: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.1") {
            let mut r = x;
            unsafe { std::arch::asm!("roundsd {0}, {0}, 9", inout(xmm_reg) r, options(pure, nomem, nostack)) };
            return r;
        }
    }
    luai_floor(x)
}

/// `ceil`, like [`luai_floor_fast`].
#[inline(always)]
pub fn luai_ceil_fast(x: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.1") {
            let mut r = x;
            unsafe { std::arch::asm!("roundsd {0}, {0}, 10", inout(xmm_reg) r, options(pure, nomem, nostack)) };
            return r;
        }
    }
    -luai_floor(-x)
}

#[inline(always)]
pub fn luai_numidiv(a: f64, b: f64) -> f64 {
    luai_floor(a / b)
}
#[inline(always)]
pub fn luai_signf(v: f32) -> f32 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}
#[inline(always)]
pub fn luai_signd(v: f64) -> f64 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}
#[inline(always)]
pub fn luai_clampf(v: f32, min: f32, max: f32) -> f32 {
    let r = if v < min { min } else { v };
    if r > max {
        max
    } else {
        r
    }
}
#[inline(always)]
pub fn luai_lerpf(a: f32, b: f32, t: f32) -> f32 {
    if t == 1.0 {
        b
    } else {
        a + (b - a) * t
    }
}
/// `luai_num2unsigned`: via a 64-bit integer, like the C++ macro.
#[inline(always)]
pub fn luai_num2unsigned(n: f64) -> u32 {
    d2l_trunc(n) as u32
}

/// `(long long)d` as C++ compiles it on x86-64 (`cvttsd2si`): out of range
/// values give `i64::MIN` instead of Rust's saturating cast sequence.
#[inline(always)]
pub fn d2l_trunc(x: f64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_mm_cvttsd_si64, _mm_set_sd};
        _mm_cvttsd_si64(_mm_set_sd(x))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        x as i64
    }
}

pub fn isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

const K_POW10_TABLE_MIN: i32 = -292;

static K_POW5_TABLE: [u64; 16] = [0x8000000000000000, 0xa000000000000000, 0xc800000000000000, 0xfa00000000000000, 0x9c40000000000000, 0xc350000000000000, 0xf424000000000000, 0x9896800000000000, 0xbebc200000000000, 0xee6b280000000000, 0x9502f90000000000, 0xba43b74000000000, 0xe8d4a51000000000, 0x9184e72a00000000, 0xb5e620f480000000, 0xe35fa931a0000000];

static K_POW10_TABLE: [[u64; 3]; 39] = [
    [0xff77b1fcbebcdc4f, 0x25e8e89c13bb0f7b, 0x333443443333443b],
    [0x8dd01fad907ffc3b, 0xae3da7d97f6792e4, 0xbbb3ab3cb3ba3cbc],
    [0x9d71ac8fada6c9b5, 0x6f773fc3603db4aa, 0x4ba4bc4bb4bb4bcc],
    [0xaecc49914078536d, 0x58fae9f773886e19, 0x3ba3bc33b43b43bb],
    [0xc21094364dfb5636, 0x985915fc12f542e5, 0x33b43b43a33b33cb],
    [0xd77485cb25823ac7, 0x7d633293366b828c, 0x34b44c444343443c],
    [0xef340a98172aace4, 0x86fb897116c87c35, 0x333343333343334b],
    [0x84c8d4dfd2c63f3b, 0x29ecd9f40041e074, 0xccaccbbcbcbb4bbc],
    [0x936b9fcebb25c995, 0xcab10dd900beec35, 0x3ab3ab3ab3bb3bbb],
    [0xa3ab66580d5fdaf5, 0xc13e60d0d2e0ebbb, 0x4cc3dc4db4db4dbb],
    [0xb5b5ada8aaff80b8, 0x0d819992132456bb, 0x33b33a34c33b34ab],
    [0xc9bcff6034c13052, 0xfc89b393dd02f0b6, 0x33c33b44b43c34bc],
    [0xdff9772470297ebd, 0x59787e2b93bc56f8, 0x43b444444443434c],
    [0xf8a95fcf88747d94, 0x75a44c6397ce912b, 0x443334343443343b],
    [0x8a08f0f8bf0f156b, 0x1b8e9ecb641b5900, 0xbbabab3aa3ab4ccc],
    [0x993fe2c6d07b7fab, 0xe546a8038efe402a, 0x4cb4bc4db4db4bcc],
    [0xaa242499697392d2, 0xdde50bd1d5d0b9ea, 0x3ba3ba3bb33b33bc],
    [0xbce5086492111aea, 0x88f4bb1ca6bcf585, 0x44b44c44c44c43cb],
    [0xd1b71758e219652b, 0xd3c36113404ea4a9, 0x44c44c44c444443b],
    [0xe8d4a51000000000, 0x0000000000000000, 0x444444444444444c],
    [0x813f3978f8940984, 0x4000000000000000, 0xcccccccccccccccc],
    [0x8f7e32ce7bea5c6f, 0xe4820023a2000000, 0xbba3bc4cc4cc4ccc],
    [0x9f4f2726179a2245, 0x01d762422c946591, 0x4aa3bb3aa3ba3bab],
    [0xb0de65388cc8ada8, 0x3b25a55f43294bcc, 0x3ca33b33b44b43bc],
    [0xc45d1df942711d9a, 0x3ba5d0bd324f8395, 0x44c44c34c44b44cb],
    [0xda01ee641a708de9, 0xe80e6f4820cc9496, 0x33b33b343333333c],
    [0xf209787bb47d6b84, 0xc0678c5dbd23a49b, 0x443444444443443b],
    [0x865b86925b9bc5c2, 0x0b8a2392ba45a9b3, 0xdbccbcccb4cb3bbb],
    [0x952ab45cfa97a0b2, 0xdd945a747bf26184, 0x3bc4bb4ab3ca3cbc],
    [0xa59bc234db398c25, 0x43fab9837e699096, 0x3bb3ac3ab3bb33ac],
    [0xb7dcbf5354e9bece, 0x0c11ed6d538aeb30, 0x33b43b43b34c34dc],
    [0xcc20ce9bd35c78a5, 0x31ec038df7b441f5, 0x34c44c43c44b44cb],
    [0xe2a0b5dc971f303a, 0x2e44ae64840fd61e, 0x333333333333333c],
    [0xfb9b7cd9a4a7443c, 0x169840ef017da3b2, 0x433344443333344c],
    [0x8bab8eefb6409c1a, 0x1ad089b6c2f7548f, 0xdcbdcc3cc4cc4bcb],
    [0x9b10a4e5e9913128, 0xca7cf2b4191c8327, 0x3ab3cb3bc3bb4bbb],
    [0xac2820d9623bf429, 0x546345fa9fbdcd45, 0x3bb3cc43c43c43cb],
    [0xbf21e44003acdd2c, 0xe0470a63e6bd56c4, 0x44b34a43b44c44bc],
    [0xd433179d9c8cb841, 0x5fa60692a46151ec, 0x43a33a33a333333c],
];

static K_DIGIT_TABLE: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

#[inline(always)]
fn mul128(x: u64, y: u64) -> (u64, u64) {
    let r = (x as u128) * (y as u128);
    (r as u64, (r >> 64) as u64)
}

#[inline(always)]
fn mul192hi(xhi: u64, xlo: u64, y: u64) -> (u64, u64) {
    let (mut z1, mut z2) = mul128(xhi, y);
    let (_z0, z1c) = mul128(xlo, y);
    z1 = z1.wrapping_add(z1c);
    z2 = z2.wrapping_add((z1 < z1c) as u64);
    (z1, z2)
}

#[inline(always)]
fn roundodd(ghi: u64, glo: u64, cp: u64) -> u64 {
    let (_xlo, xhi) = mul128(glo, cp);
    let (ylo, yhi) = mul128(ghi, cp);
    let z = ylo.wrapping_add(xhi);
    (yhi.wrapping_add((z < xhi) as u64)) | ((z > 1) as u64)
}

fn schubfach(exponent: i32, fraction: u64) -> (u64, i32) {
    let mut c = fraction;
    let mut q = exponent - 1023 - 51;
    if exponent != 0 {
        c |= 1u64 << 52;
        q -= 1;
    }
    if ((-q) as u32) < 53 && (c & ((1u64 << (-q)) - 1)) == 0 {
        return (c >> (-q), 0);
    }
    let irr = (c == (1u64 << 52) && q != -1074) as u64;
    let out = c & 1;
    let cbl = 4 * c - 2 + irr;
    let cb = 4 * c;
    let cbr = 4 * c + 2;
    const Q: i32 = 20;
    const C: i32 = 315652;
    const A: i32 = -131008;
    const C2: i32 = 3483294;
    let k = (q * C + if irr != 0 { A } else { 0 }) >> Q;
    let h = q + ((-k * C2) >> Q) + 1;
    let gtoff = -k - K_POW10_TABLE_MIN;
    let gt = &K_POW10_TABLE[(gtoff >> 4) as usize];
    let (mut glo, mut ghi) = mul192hi(gt[0], gt[1], K_POW5_TABLE[(gtoff & 15) as usize]);
    let gterr = ((gt[2] >> ((gtoff & 15) * 4)) & 15) as i32;
    let gtscale = (gterr >> 3) as u32;
    ghi <<= gtscale;
    ghi = ghi.wrapping_add((glo >> 63) & gtscale as u64);
    glo <<= gtscale;
    glo = glo.wrapping_sub(((gterr & 7) - 4) as i64 as u64);
    let vbl = roundodd(ghi, glo, cbl << h);
    let vb = roundodd(ghi, glo, cb << h);
    let vbr = roundodd(ghi, glo, cbr << h);
    let s = vb / 4;
    if s >= 10 {
        let sp = s / 10;
        let upin = vbl + out <= 40 * sp;
        let wpin = vbr >= 40 * sp + 40 + out;
        if upin != wpin {
            return (sp + wpin as u64, k + 1);
        }
    }
    let uin = vbl + out <= 4 * s;
    let win = 4 * s + 4 + out <= vbr;
    let rup = vb >= 4 * s + 2 + 1 - (s & 1);
    (s + if uin != win { win as u64 } else { rup as u64 }, k)
}

fn printunsignedrev(buf: &mut [u8], mut end: usize, mut num: u64) -> usize {
    while num >= 10000 {
        let tail = (num % 10000) as usize;
        buf[end - 4..end - 2].copy_from_slice(&K_DIGIT_TABLE[(tail / 100) * 2..(tail / 100) * 2 + 2]);
        buf[end - 2..end].copy_from_slice(&K_DIGIT_TABLE[(tail % 100) * 2..(tail % 100) * 2 + 2]);
        num /= 10000;
        end -= 4;
    }
    let mut rest = num as usize;
    while rest >= 10 {
        buf[end - 2..end].copy_from_slice(&K_DIGIT_TABLE[(rest % 100) * 2..(rest % 100) * 2 + 2]);
        rest /= 100;
        end -= 2;
    }
    if rest != 0 {
        buf[end - 1] = b'0' + rest as u8;
        end -= 1;
    }
    end
}

/// `luai_num2str`: shortest representation that round-trips.
/// Byte sink for number formatting: a `Vec` or a stack [`NumBuf`].
pub trait NumSink {
    fn push(&mut self, c: u8);
    fn extend_from_slice(&mut self, s: &[u8]);
    fn last(&self) -> Option<&u8>;
    fn pop(&mut self) -> Option<u8>;
}

impl NumSink for Vec<u8> {
    #[inline(always)]
    fn push(&mut self, c: u8) {
        Vec::push(self, c)
    }
    #[inline(always)]
    fn extend_from_slice(&mut self, s: &[u8]) {
        Vec::extend_from_slice(self, s)
    }
    #[inline(always)]
    fn last(&self) -> Option<&u8> {
        <[u8]>::last(self)
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<u8> {
        Vec::pop(self)
    }
}

/// Fixed stack buffer for one formatted number (no heap allocation).
pub struct NumBuf {
    /// only `..len` is ever read, so the rest stays uninitialized (no
    /// 64-byte fill per number formatted)
    buf: std::mem::MaybeUninit<[u8; 64]>,
    len: usize,
}

impl NumBuf {
    #[inline(always)]
    pub fn new() -> Self {
        NumBuf { buf: std::mem::MaybeUninit::uninit(), len: 0 }
    }
    #[inline(always)]
    fn ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr() as *mut u8
    }
    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.len) }
    }
}

impl Default for NumBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl NumSink for NumBuf {
    #[inline(always)]
    fn push(&mut self, c: u8) {
        assert!(self.len < 64);
        unsafe { *self.ptr().add(self.len) = c };
        self.len += 1;
    }
    #[inline(always)]
    fn extend_from_slice(&mut self, s: &[u8]) {
        assert!(self.len + s.len() <= 64);
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), self.ptr().add(self.len), s.len()) };
        self.len += s.len();
    }
    #[inline(always)]
    fn last(&self) -> Option<&u8> {
        self.as_slice().last()
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            // (a byte written before: it was below the old length)
            Some(unsafe { *self.ptr().add(self.len) })
        }
    }
}

/// `luai_num2str` into a caller's stack buffer (filled in place: returning a
/// freshly written buffer by value would copy it with wide loads right after
/// byte stores, which stalls on store forwarding).
#[inline(always)]
pub fn luai_num2buf(b: &mut NumBuf, n: f64) -> &[u8] {
    b.len = 0;
    luai_num2str(b, n);
    b.as_slice()
}

/// `luai_int2str` into a caller's stack buffer.
#[inline(always)]
pub fn luai_int2buf(b: &mut NumBuf, l: i64) -> &[u8] {
    b.len = 0;
    luai_int2str(b, l);
    b.as_slice()
}

pub fn luai_num2str<S: NumSink>(out: &mut S, n: f64) {
    let bits = n.to_bits();
    let sign = (bits >> 63) as usize;
    let exponent = ((bits >> 52) & 2047) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    if exponent == 0x7ff {
        if fraction == 0 {
            out.extend_from_slice(if sign != 0 { b"-inf" } else { b"inf" });
        } else {
            out.extend_from_slice(b"nan");
        }
        return;
    }
    if sign != 0 {
        out.push(b'-');
    }
    if exponent == 0 && fraction == 0 {
        out.push(b'0');
        return;
    }
    let (s, k) = schubfach(exponent, fraction);
    let mut decbuf = [0u8; 40];
    let decend = 20;
    let dec = printunsignedrev(&mut decbuf, decend, s);
    let digits = &decbuf[dec..decend];
    let declen = digits.len() as i32;
    let dot = declen + k;
    fn trim<S: NumSink>(out: &mut S) {
        while out.last() == Some(&b'0') {
            out.pop();
        }
    }
    if (-5..=21).contains(&dot) {
        if dot <= 0 {
            out.extend_from_slice(b"0.");
            for _ in 0..(-dot) {
                out.push(b'0');
            }
            out.extend_from_slice(digits);
            trim(out);
        } else if dot == declen {
            out.extend_from_slice(digits);
        } else if dot < declen {
            out.extend_from_slice(&digits[..dot as usize]);
            out.push(b'.');
            out.extend_from_slice(&digits[dot as usize..]);
            trim(out);
        } else {
            out.extend_from_slice(digits);
            for _ in 0..(dot - declen) {
                out.push(b'0');
            }
        }
    } else {
        out.push(digits[0]);
        out.push(b'.');
        out.extend_from_slice(&digits[1..]);
        trim(out);
        if out.last() == Some(&b'.') {
            out.pop();
        }
        let e = dot - 1;
        out.push(b'e');
        out.push(if e < 0 { b'-' } else { b'+' });
        let mut v = e.unsigned_abs() as usize;
        if v >= 100 {
            out.push(b'0' + (v / 100) as u8);
            v %= 100;
        }
        out.extend_from_slice(&K_DIGIT_TABLE[v * 2..v * 2 + 2]);
    }
}

/// `luai_int2str`.
pub fn luai_int2str<S: NumSink>(out: &mut S, l: i64) {
    let mut tmp = [0u8; 24];
    let mut i = tmp.len();
    let mut v = l.unsigned_abs();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if l < 0 {
        i -= 1;
        tmp[i] = b'-';
    }
    out.extend_from_slice(&tmp[i..]);
}

// ---------------------------------------------------------------------------
// C library conversions
// ---------------------------------------------------------------------------

fn digit_value(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'z' => (c - b'a') as u32 + 10,
        b'A'..=b'Z' => (c - b'A') as u32 + 10,
        _ => 99,
    }
}

/// Parses `[space][sign] digits` per `strtoull` (with an optional `0x`
/// prefix for base 16 / base 0). Returns (magnitude, negative, overflow,
/// end); end == 0 when no conversion was performed.
fn parse_integer(s: &[u8], mut base: u32) -> (u64, bool, bool, usize) {
    let mut i = 0;
    while i < s.len() && isspace(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    if (base == 0 || base == 16)
        && i + 2 < s.len()
        && s[i] == b'0'
        && (s[i + 1] == b'x' || s[i + 1] == b'X')
        && digit_value(s[i + 2]) < 16
    {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if i < s.len() && s[i] == b'0' { 8 } else { 10 };
    }
    let start = i;
    let mut v: u64 = 0;
    let mut overflow = false;
    while i < s.len() {
        let d = digit_value(s[i]);
        if d >= base {
            break;
        }
        match v.checked_mul(base as u64).and_then(|x| x.checked_add(d as u64)) {
            Some(x) => v = x,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return (0, false, false, 0);
    }
    (v, neg, overflow, i)
}

/// `strtoull`: returns (value, end).
pub fn strtoull(s: &[u8], base: u32) -> (u64, usize) {
    let (v, neg, overflow, end) = parse_integer(s, base);
    if end == 0 {
        return (0, 0);
    }
    if overflow {
        return (u64::MAX, end);
    }
    (if neg { v.wrapping_neg() } else { v }, end)
}

/// `strtoul` (callers only use the value as a double).
pub fn strtoul(s: &[u8], base: u32) -> (u64, usize) {
    strtoull(s, base)
}

/// `strtoll`: saturating signed conversion.
pub fn strtoll(s: &[u8], base: u32) -> (i64, usize) {
    let (v, neg, overflow, end) = parse_integer(s, base);
    if end == 0 {
        return (0, 0);
    }
    if neg {
        if overflow || v > (i64::MAX as u64) + 1 {
            return (i64::MIN, end);
        }
        ((v as i64).wrapping_neg(), end)
    } else {
        if overflow || v > i64::MAX as u64 {
            return (i64::MAX, end);
        }
        (v as i64, end)
    }
}

/// `strtod`: decimal/hex floats, `inf`/`infinity`/`nan`. Returns (value,
/// end); end == 0 when no conversion was performed.
pub fn strtod(s: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < s.len() && isspace(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let starts = |w: &[u8]| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
    if starts(b"infinity") {
        return (if neg { f64::NEG_INFINITY } else { f64::INFINITY }, i + 8);
    }
    if starts(b"inf") {
        return (if neg { f64::NEG_INFINITY } else { f64::INFINITY }, i + 3);
    }
    if starts(b"nan") {
        let mut e = i + 3;
        if e < s.len() && s[e] == b'(' {
            let mut j = e + 1;
            while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'_') {
                j += 1;
            }
            if j < s.len() && s[j] == b')' {
                e = j + 1;
            }
        }
        return (if neg { -f64::NAN } else { f64::NAN }, e);
    }
    if rest.len() >= 2 && rest[0] == b'0' && (rest[1] == b'x' || rest[1] == b'X') {
        return strtod_hex(rest, i, neg);
    }
    let mut j = 0;
    let mut digits = false;
    while j < rest.len() && rest[j].is_ascii_digit() {
        j += 1;
        digits = true;
    }
    if j < rest.len() && rest[j] == b'.' {
        let mut k = j + 1;
        let mut fd = false;
        while k < rest.len() && rest[k].is_ascii_digit() {
            k += 1;
            fd = true;
        }
        if digits || fd {
            digits = true;
            j = k;
        }
    }
    if !digits {
        return (0.0, 0);
    }
    if j < rest.len() && (rest[j] == b'e' || rest[j] == b'E') {
        let mut k = j + 1;
        if k < rest.len() && (rest[k] == b'+' || rest[k] == b'-') {
            k += 1;
        }
        if k < rest.len() && rest[k].is_ascii_digit() {
            while k < rest.len() && rest[k].is_ascii_digit() {
                k += 1;
            }
            j = k;
        }
    }
    let num = &rest[..j];
    // plain integers of up to 15 digits are exact in a double
    if j <= 15 && num.iter().all(|c| c.is_ascii_digit()) {
        let mut u: u64 = 0;
        for &c in num {
            u = u * 10 + (c - b'0') as u64;
        }
        let v = u as f64;
        return (if neg { -v } else { v }, i + j);
    }
    // `num` is ASCII ([digits][.digits][e[+-]digits]), which Rust's correctly
    // rounded parser accepts as is ("5.", ".5" and "1.e5" included)
    let v: f64 = unsafe { std::str::from_utf8_unchecked(num) }.parse().unwrap_or(0.0);
    (if neg { -v } else { v }, i + j)
}

/// Hexadecimal floating point (`0x1.8p3`), correctly rounded.
fn strtod_hex(rest: &[u8], i: usize, neg: bool) -> (f64, usize) {
    let mut j = 2;
    // collect up to 16 significant hex digits plus a sticky bit
    let mut mant: u64 = 0;
    let mut sticky = false;
    let mut exp: i64 = 0;
    let mut any = false;
    let mut nonzero = false;
    let mut push = |d: u32, mant: &mut u64, exp: &mut i64, frac: bool, sticky: &mut bool, nonzero: &mut bool| {
        if !*nonzero && d == 0 {
            if frac {
                *exp -= 4;
            }
            return;
        }
        *nonzero = true;
        if *mant >> 60 == 0 {
            *mant = (*mant << 4) | d as u64;
            if frac {
                *exp -= 4;
            }
        } else {
            if d != 0 {
                *sticky = true;
            }
            if !frac {
                *exp += 4;
            }
        }
    };
    while j < rest.len() && digit_value(rest[j]) < 16 {
        any = true;
        push(digit_value(rest[j]), &mut mant, &mut exp, false, &mut sticky, &mut nonzero);
        j += 1;
    }
    if j < rest.len() && rest[j] == b'.' {
        let mut k = j + 1;
        let mut anyf = false;
        while k < rest.len() && digit_value(rest[k]) < 16 {
            anyf = true;
            push(digit_value(rest[k]), &mut mant, &mut exp, true, &mut sticky, &mut nonzero);
            k += 1;
        }
        if any || anyf {
            any = true;
            j = k;
        }
    }
    if !any {
        return (if neg { -0.0 } else { 0.0 }, i + 1);
    }
    if j < rest.len() && (rest[j] == b'p' || rest[j] == b'P') {
        let mut k = j + 1;
        let mut eneg = false;
        if k < rest.len() && (rest[k] == b'+' || rest[k] == b'-') {
            eneg = rest[k] == b'-';
            k += 1;
        }
        if k < rest.len() && rest[k].is_ascii_digit() {
            let mut e: i64 = 0;
            while k < rest.len() && rest[k].is_ascii_digit() {
                e = (e * 10 + (rest[k] - b'0') as i64).min(1 << 30);
                k += 1;
            }
            exp += if eneg { -e } else { e };
            j = k;
        }
    }
    let v = compose_binary(mant, sticky, exp);
    (if neg { -v } else { v }, i + j)
}

/// mant * 2^exp rounded to nearest-even (sticky marks discarded low bits).
fn compose_binary(mant: u64, sticky: bool, exp: i64) -> f64 {
    if mant == 0 {
        return 0.0;
    }
    // normalize to 64 bits
    let lz = mant.leading_zeros() as i64;
    let m = mant << lz;
    let e = exp - lz + 63; // value = m/2^63 * 2^e
    // target: 53-bit significand, biased exponent
    let (shift, biased) = if e + 1023 >= 1 {
        (11i64, e + 1023)
    } else {
        // subnormal
        (11 + (1 - (e + 1023)), 0)
    };
    if biased >= 2047 {
        return f64::INFINITY;
    }
    if shift >= 64 + 11 {
        return 0.0;
    }
    let (kept, rem_bits) = if shift >= 64 { (0u64, m) } else { (m >> shift, m << (64 - shift)) };
    let half = 1u64 << 63;
    let round_up = if shift >= 64 {
        false
    } else if rem_bits > half || (rem_bits == half && (sticky || kept & 1 == 1)) {
        true
    } else {
        rem_bits == half && false
    };
    let round_up = round_up || (rem_bits == half && sticky);
    let mut kept = kept + round_up as u64;
    let mut biased = biased;
    if biased == 0 {
        // subnormal: kept is the raw fraction; may round into normal
        return f64::from_bits(kept);
    }
    if kept >> 53 != 0 {
        kept >>= 1;
        biased += 1;
        if biased >= 2047 {
            return f64::INFINITY;
        }
    }
    f64::from_bits(((biased as u64) << 52) | (kept & ((1u64 << 52) - 1)))
}
