//! C `snprintf` semantics for the conversions `string.format` uses
//! (`d i o u x X c e E f F g G s`) with flags `-+ #0`, width and precision.

#[derive(Default, Clone, Copy)]
pub struct Spec {
    pub minus: bool,
    pub plus: bool,
    pub space: bool,
    pub alt: bool,
    pub zero: bool,
    pub width: usize,
    pub prec: Option<usize>,
}

/// Parses `%[flags][width][.prec]` content (without `%` and conversion).
pub fn parse_spec(s: &[u8]) -> Spec {
    let mut sp = Spec::default();
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'-' => sp.minus = true,
            b'+' => sp.plus = true,
            b' ' => sp.space = true,
            b'#' => sp.alt = true,
            b'0' => sp.zero = true,
            _ => break,
        }
        i += 1;
    }
    let mut w = 0usize;
    while i < s.len() && s[i].is_ascii_digit() {
        w = w * 10 + (s[i] - b'0') as usize;
        i += 1;
    }
    sp.width = w;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let mut p = 0usize;
        while i < s.len() && s[i].is_ascii_digit() {
            p = p * 10 + (s[i] - b'0') as usize;
            i += 1;
        }
        sp.prec = Some(p);
    }
    sp
}

fn pad(out: &mut Vec<u8>, sp: &Spec, sign: &[u8], body: &[u8], zero_allowed: bool) {
    let len = sign.len() + body.len();
    if len >= sp.width {
        out.extend_from_slice(sign);
        out.extend_from_slice(body);
    } else if sp.minus {
        out.extend_from_slice(sign);
        out.extend_from_slice(body);
        out.resize(out.len() + sp.width - len, b' ');
    } else if sp.zero && zero_allowed {
        out.extend_from_slice(sign);
        out.resize(out.len() + sp.width - len, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + sp.width - len, b' ');
        out.extend_from_slice(sign);
        out.extend_from_slice(body);
    }
}

/// `%d`/`%i` of a 64-bit value.
/// Digits of `v` in `base`, right-aligned in `buf` and zero-padded to
/// `prec` digits (none at all for 0 with precision 0, as C does); returns
/// the start index. No allocation (precision is at most 99).
fn digits_into(buf: &mut [u8; 128], v: u64, base: u64, upper: bool, prec: Option<usize>) -> usize {
    let set: &[u8; 16] = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut i = buf.len();
    if !(v == 0 && prec == Some(0)) {
        // a constant base per call site, so `%` and `/` become a mask/shift
        // (8, 16) or a multiply (10) instead of a 64-bit divide per digit
        #[inline(always)]
        fn digits<const B: u64>(buf: &mut [u8; 128], mut i: usize, mut x: u64, set: &[u8; 16]) -> usize {
            loop {
                i -= 1;
                buf[i] = set[(x % B) as usize];
                x /= B;
                if x == 0 {
                    return i;
                }
            }
        }
        i = match base {
            16 => digits::<16>(buf, i, v, set),
            10 => digits::<10>(buf, i, v, set),
            8 => digits::<8>(buf, i, v, set),
            _ => {
                let mut x = v;
                loop {
                    i -= 1;
                    buf[i] = set[(x % base) as usize];
                    x /= base;
                    if x == 0 {
                        break i;
                    }
                }
            }
        };
    }
    if let Some(p) = prec {
        let p = p.min(buf.len() - 1);
        while buf.len() - i < p {
            i -= 1;
            buf[i] = b'0';
        }
    }
    i
}

pub fn fmt_int(out: &mut Vec<u8>, sp: &Spec, v: i64) {
    let neg = v < 0;
    let mag = v.unsigned_abs();
    let mut buf = [0u8; 128];
    let start = digits_into(&mut buf, mag, 10, false, sp.prec);
    let digits = &buf[start..];
    let sign: &[u8] = if neg {
        b"-"
    } else if sp.plus {
        b"+"
    } else if sp.space {
        b" "
    } else {
        b""
    };
    pad(out, sp, sign, digits, sp.prec.is_none());
}

/// `%o %u %x %X` of a 64-bit unsigned value.
pub fn fmt_uint(out: &mut Vec<u8>, sp: &Spec, v: u64, conv: u8) {
    let base = match conv {
        b'o' => 8,
        b'x' | b'X' => 16,
        _ => 10,
    };
    let mut buf = [0u8; 128];
    let mut start = digits_into(&mut buf, v, base, conv == b'X', sp.prec);
    let mut prefix: &[u8] = b"";
    if sp.alt {
        match conv {
            b'o' => {
                if start < buf.len() && buf[start] != b'0' {
                    start -= 1;
                    buf[start] = b'0';
                }
            }
            b'x' if v != 0 => prefix = b"0x",
            b'X' if v != 0 => prefix = b"0X",
            _ => {}
        }
    }
    pad(out, sp, prefix, &buf[start..], sp.prec.is_none());
}

/// `%c`.
pub fn fmt_char(out: &mut Vec<u8>, sp: &Spec, c: u8) {
    pad(out, sp, b"", &[c], false);
}

/// `%s`.
pub fn fmt_str(out: &mut Vec<u8>, sp: &Spec, s: &[u8]) {
    let s = match sp.prec {
        Some(p) if p < s.len() => &s[..p],
        _ => s,
    };
    pad(out, sp, b"", s, false);
}

/// Rust `{:e}` output ("1.5e-7") to C form ("1.5e-07").
fn fix_exp(s: String, upper: bool) -> Vec<u8> {
    let (mant, exp) = match s.find('e') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (&s[..], "0"),
    };
    let (esign, edigits) = if let Some(stripped) = exp.strip_prefix('-') { ('-', stripped) } else { ('+', exp) };
    let mut out = mant.as_bytes().to_vec();
    out.push(if upper { b'E' } else { b'e' });
    out.push(esign as u8);
    if edigits.len() < 2 {
        out.push(b'0');
    }
    out.extend_from_slice(edigits.as_bytes());
    out
}

fn exp_of(s: &str) -> i32 {
    s.find('e').map(|i| s[i + 1..].parse().unwrap_or(0)).unwrap_or(0)
}

/// `%e %E %f %F %g %G`.
/// `%.{prec}f` of a finite non-negative `a` by exact integer arithmetic
/// (a = m * 2^e: m * 10^prec >> -e, rounded half to even on the exact
/// remainder - what C's printf does); false when out of its range.
fn fixed_exact(out: &mut Vec<u8>, a: f64, prec: usize) -> bool {
    if prec > 9 {
        return false;
    }
    let bits = a.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) };
    let p10 = 10u128.pow(prec as u32);
    let q: u128 = if m == 0 {
        0
    } else if e >= 0 {
        if e > 10 {
            return false;
        }
        ((m as u128) << e) * p10
    } else {
        let shift = (-e) as u32;
        if shift > 120 {
            return false;
        }
        let n = m as u128 * p10;
        let q = n >> shift;
        let r = n & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        if r > half || (r == half && q & 1 == 1) { q + 1 } else { q }
    };
    let int = q / p10;
    let mut f = (q % p10) as u64;
    let mut buf = [0u8; 48];
    let mut k = buf.len();
    let mut iv = int;
    loop {
        k -= 1;
        buf[k] = b'0' + (iv % 10) as u8;
        iv /= 10;
        if iv == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[k..]);
    if prec > 0 {
        out.push(b'.');
        let mut d = [b'0'; 9];
        for j in (0..prec).rev() {
            d[j] = b'0' + (f % 10) as u8;
            f /= 10;
        }
        out.extend_from_slice(&d[..prec]);
    }
    true
}

pub fn fmt_float(out: &mut Vec<u8>, sp: &Spec, v: f64, conv: u8) {
    let upper = conv.is_ascii_uppercase();
    let neg = v.is_sign_negative() && !v.is_nan();
    let sign: &[u8] = if neg {
        b"-"
    } else if sp.plus {
        b"+"
    } else if sp.space {
        b" "
    } else {
        b""
    };
    let a = v.abs();
    if !a.is_finite() {
        let body: &[u8] = match (a.is_nan(), upper) {
            (true, false) => b"nan",
            (true, true) => b"NAN",
            (false, false) => b"inf",
            (false, true) => b"INF",
        };
        let sign: &[u8] = if v.is_nan() { if sp.plus { b"+" } else if sp.space { b" " } else { b"" } } else { sign };
        pad(out, sp, sign, body, false);
        return;
    }
    let prec = sp.prec.unwrap_or(6);
    if conv == b'f' && sp.width == 0 && !sp.alt && !sp.plus && !sp.space && prec <= 40 {
        // (the common %.Nf: formatted straight into the output, no
        // allocation)
        struct Out<'a>(&'a mut Vec<u8>);
        impl std::fmt::Write for Out<'_> {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                self.0.extend_from_slice(s.as_bytes());
                Ok(())
            }
        }
        if neg {
            out.push(b'-');
        }
        if fixed_exact(out, a, prec) {
            return;
        }
        let _ = std::fmt::Write::write_fmt(&mut Out(out), format_args!("{:.*}", prec, a));
        return;
    }
    let body: Vec<u8> = match conv.to_ascii_lowercase() {
        b'e' => {
            let mut b = fix_exp(format!("{:.*e}", prec, a), upper);
            if sp.alt && prec == 0 {
                let i = b.iter().position(|&c| c == b'e' || c == b'E').unwrap();
                b.insert(i, b'.');
            }
            b
        }
        b'f' => {
            let mut b = format!("{:.*}", prec, a).into_bytes();
            if sp.alt && prec == 0 {
                b.push(b'.');
            }
            b
        }
        _ => {
            let p = if prec == 0 { 1 } else { prec };
            let es = format!("{:.*e}", p - 1, a);
            let x = exp_of(&es);
            let mut b = if (p as i32) > x && x >= -4 {
                format!("{:.*}", (p as i32 - 1 - x) as usize, a).into_bytes()
            } else {
                fix_exp(es, upper)
            };
            if !sp.alt {
                // strip trailing zeros of the fraction (and a bare point)
                let epos = b.iter().position(|&c| c == b'e' || c == b'E').unwrap_or(b.len());
                let (m, e) = b.split_at(epos);
                let mut m = m.to_vec();
                if m.contains(&b'.') {
                    while m.last() == Some(&b'0') {
                        m.pop();
                    }
                    if m.last() == Some(&b'.') {
                        m.pop();
                    }
                }
                m.extend_from_slice(e);
                b = m;
            } else if !b.contains(&b'.') {
                let epos = b.iter().position(|&c| c == b'e' || c == b'E').unwrap_or(b.len());
                b.insert(epos, b'.');
            }
            b
        }
    };
    pad(out, sp, sign, &body, true);
}
