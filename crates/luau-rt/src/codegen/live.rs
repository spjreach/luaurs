//! Register liveness over the bytecode: for every instruction, the set of VM
//! registers whose current value may still be read (by that instruction or
//! any later one). The translator only writes lazily held values back to the
//! VM stack when they are live, so temporaries that die inside a block never
//! touch memory.
//!
//! Uses are over-approximated and definitions under-approximated, so a wrong
//! guess can only cost a store, never drop one. Functions containing an
//! opcode this analysis does not model get "everything is live".

use luau_common::bytecode::{get_op_length, Opcode as O};

pub type Mask = [u64; 4];
pub const ALL: Mask = [u64::MAX; 4];

#[inline]
pub fn has(m: &Mask, r: usize) -> bool {
    m[r >> 6] & (1 << (r & 63)) != 0
}
#[inline]
fn set(m: &mut Mask, r: usize) {
    if r < 256 {
        m[r >> 6] |= 1 << (r & 63);
    }
}
fn set_from(m: &mut Mask, lo: usize) {
    for r in lo..256 {
        set(m, r);
    }
}
fn set_range(m: &mut Mask, lo: usize, hi: usize) {
    for r in lo..hi.min(256) {
        set(m, r);
    }
}

pub struct Info {
    pub uses: Mask,
    pub defs: Mask,
    /// registers written only on the way to `succ[1]` (a builtin's results)
    pub defs1: Mask,
    /// successors (instruction indices); `usize::MAX` entries are unused
    pub succ: [usize; 2],
}

/// Use/def/successor model of the instruction at `i`; None for opcodes that
/// are not modelled.
pub unsafe fn info(code: *const u32, n: usize, i: usize) -> Option<Info> {
    let insn = *code.add(i);
    let op = (insn & 0xff) as u8;
    let o = O::try_from(op).ok()?;
    let len = get_op_length(o) as usize;
    let a = ((insn >> 8) & 0xff) as usize;
    let b = ((insn >> 16) & 0xff) as usize;
    let c = ((insn >> 24) & 0xff) as usize;
    let d = (insn as i32) >> 16;
    let aux = if len > 1 { *code.add(i + 1) } else { 0 };
    let mut u: Mask = [0; 4];
    let mut df: Mask = [0; 4];
    let mut df1: Mask = [0; 4];
    let next = i + len;
    let jump = (i as isize + 1 + d as isize) as usize;
    let mut succ = [next, usize::MAX];
    match o {
        O::Nop | O::Coverage | O::PrepVarargs | O::CloseUpvals | O::NativeCall => {}
        O::LoadNil | O::LoadN | O::LoadK | O::LoadKx | O::GetGlobal | O::GetUpval | O::GetImport | O::NewTable | O::DupTable
        | O::NewClosure | O::DupClosure => set(&mut df, a),
        O::LoadB => {
            set(&mut df, a);
            if c != 0 {
                succ = [i + 1 + c, usize::MAX];
            }
        }
        O::Move | O::Not | O::Minus | O::Length | O::GetTableKs | O::GetTableN | O::AddK | O::SubK | O::MulK | O::DivK | O::ModK
        | O::PowK | O::IDivK | O::AndK | O::OrK => {
            set(&mut u, b);
            set(&mut df, a);
        }
        O::SubRk | O::DivRk => {
            set(&mut u, c);
            set(&mut df, a);
        }
        O::GetTable | O::Add | O::Sub | O::Mul | O::Div | O::Mod | O::Pow | O::IDiv | O::And | O::Or => {
            set(&mut u, b);
            set(&mut u, c);
            set(&mut df, a);
        }
        O::SetGlobal | O::SetUpval => set(&mut u, a),
        O::SetTable => {
            set(&mut u, a);
            set(&mut u, b);
            set(&mut u, c);
        }
        O::SetTableKs | O::SetTableN => {
            set(&mut u, a);
            set(&mut u, b);
        }
        O::Namecall => {
            set(&mut u, b);
            set(&mut df, a);
            set(&mut df, a + 1);
        }
        O::Call | O::CallFb => {
            if b == 0 {
                set_from(&mut u, a);
            } else {
                set_range(&mut u, a, a + b);
            }
            if c != 0 {
                set_range(&mut df, a, a + c - 1);
            } else {
                // results up to L->top: R(A) and everything above is
                // overwritten (the frame above A is the callee's), so a
                // later variadic use doesn't keep the whole function's
                // upper registers live
                set_from(&mut df, a);
            }
        }
        O::Return => {
            if b == 0 {
                set_from(&mut u, a);
            } else {
                set_range(&mut u, a, a + b - 1);
            }
            succ = [usize::MAX, usize::MAX];
        }
        O::Jump | O::JumpBack => succ = [jump, usize::MAX],
        O::JumpX => succ = [(i as isize + 1 + ((insn as i32) >> 8) as isize) as usize, usize::MAX],
        O::JumpIf | O::JumpIfNot | O::JumpXEqKNil | O::JumpXEqKB | O::JumpXEqKN | O::JumpXEqKS => {
            set(&mut u, a);
            succ = [next, jump];
        }
        O::JumpIfEq | O::JumpIfLe | O::JumpIfLt | O::JumpIfNotEq | O::JumpIfNotLe | O::JumpIfNotLt => {
            set(&mut u, a);
            set(&mut u, aux as usize);
            succ = [next, jump];
        }
        O::Concat => {
            set_range(&mut u, b, c + 1);
            set(&mut df, a);
        }
        O::SetList => {
            set(&mut u, a);
            if c == 0 {
                set_from(&mut u, b);
            } else {
                set_range(&mut u, b, b + c - 1);
            }
        }
        O::ForNPrep | O::ForNLoop | O::ForGLoop | O::ForGPrep | O::ForGPrepInext | O::ForGPrepNext => {
            set_range(&mut u, a, a + 3);
            succ = [next, jump];
        }
        O::GetVarargs => {
            if b != 0 {
                set_range(&mut df, a, a + b - 1);
            }
        }
        O::Capture => {
            if a != 2 {
                set(&mut u, b);
            }
        }
        O::FastCall | O::FastCall1 | O::FastCall2 | O::FastCall2K | O::FastCall3 => {
            // the arguments are used by the fallback CALL on the next path
            // too; the builtin path continues after that CALL
            let skip = match o {
                O::FastCall | O::FastCall1 => c,
                _ => c.checked_sub(1)?,
            };
            match o {
                O::FastCall => {}
                O::FastCall1 | O::FastCall2K => set(&mut u, b),
                O::FastCall2 => {
                    set(&mut u, b);
                    set(&mut u, (aux & 0xff) as usize);
                }
                _ => {
                    set(&mut u, b);
                    set(&mut u, (aux & 0xff) as usize);
                    set(&mut u, ((aux >> 8) & 0xff) as usize);
                }
            }
            let call_i = i + len + skip;
            if call_i >= n {
                return None;
            }
            let call = *code.add(call_i);
            if (call & 0xff) as u8 != O::Call as u8 {
                return None;
            }
            let ca = ((call >> 8) & 0xff) as usize;
            let cb = ((call >> 16) & 0xff) as usize;
            // only variadic FASTCALL reads the call's argument registers;
            // the others read their operands, and their fallback reaches the
            // CALL through the setup code on the `next` path
            if o == O::FastCall {
                if cb == 0 {
                    set_from(&mut u, ca + 1);
                } else {
                    set_range(&mut u, ca + 1, ca + cb);
                }
            }
            // the builtin's results, when it succeeds
            let cc = ((call >> 24) & 0xff) as usize;
            if cc != 0 {
                set_range(&mut df1, ca, ca + cc - 1);
            } else {
                // results up to L->top, as for a variadic CALL
                set_from(&mut df1, ca);
            }
            succ = [next, call_i + 1];
        }
        _ => return None,
    }
    Some(Info { uses: u, defs: df, defs1: df1, succ })
}

/// Live-in sets per instruction index (meaningful at instruction starts);
/// None when the function has an instruction that is not modelled.
pub unsafe fn analyze(code: *const u32, n: usize, starts: &[bool], always: &[u8]) -> Option<Vec<Mask>> {
    let mut infos: Vec<Option<Info>> = Vec::with_capacity(n);
    for i in 0..n {
        if starts[i] {
            let inf = info(code, n, i)?;
            for &s in &inf.succ {
                if s != usize::MAX && (s >= n || !starts[s]) {
                    return None;
                }
            }
            infos.push(Some(inf));
        } else {
            infos.push(None);
        }
    }
    let mut live: Vec<Mask> = vec![[0; 4]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..n).rev() {
            let Some(inf) = &infos[i] else { continue };
            let mut out: Mask = [0; 4];
            for (k, &s) in inf.succ.iter().enumerate() {
                if s != usize::MAX {
                    for w in 0..4 {
                        out[w] |= live[s][w] & if k == 1 { !inf.defs1[w] } else { !0 };
                    }
                }
            }
            let mut inn: Mask = [0; 4];
            for w in 0..4 {
                inn[w] = inf.uses[w] | (out[w] & !inf.defs[w]);
            }
            if inn != live[i] {
                live[i] = inn;
                changed = true;
            }
        }
    }
    // registers captured by reference are visible to other functions
    for m in live.iter_mut() {
        for &r in always {
            set(m, r as usize);
        }
    }
    Some(live)
}
