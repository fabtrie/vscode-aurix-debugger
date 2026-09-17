//! Cortex-M (Thumb-2) call stack unwinding by prologue analysis.
//!
//! The DWARF CFI of the GHS ARM compiler describes no register rules (every
//! FDE is empty), so, as for TriCore, the stack is unwound without it: the
//! prologue of each function (from its start up to the PC) is decoded for the
//! instructions that move SP and save registers (`push`, `push.w`,
//! `str.w rN, [sp, #-4]!`, `sub sp`, `sub.w sp`, `subw sp`). The caller's SP is
//! the SP at function entry, its PC the saved LR - or LR itself in the top
//! frame of a function that has not saved it (yet). A return address with an
//! EXC_RETURN value continues in the exception frame stacked on MSP or PSP.

use crate::unwind::Frame;

pub const R_SP: usize = 13;
pub const R_LR: usize = 14;
pub const R_PC: usize = 15;
/// Frame register slots for the banked stack pointers (not DWARF register numbers in use).
pub const R_MSP: usize = 17;
pub const R_PSP: usize = 18;

/// Longest prologue decoded.
const MAX_PROLOGUE: u32 = 128;
const EXC_RETURN_MIN: u32 = 0xFFFF_FFF0;

/// Stack effect of the prologue instructions executed before the PC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prologue {
    /// Bytes allocated on the stack since function entry.
    pub stack: u32,
    /// Saved registers with their offset below the entry SP (address = entry SP - offset).
    pub saved: Vec<(usize, u32)>,
}

/// ThumbExpandImm (ARMv7-M A5.3.2).
fn thumb_expand_imm(imm12: u32) -> u32 {
    let imm8 = imm12 & 0xFF;
    if imm12 >> 10 == 0 {
        match (imm12 >> 8) & 3 {
            0 => imm8,
            1 => imm8 << 16 | imm8,
            2 => imm8 << 24 | imm8 << 8,
            _ => imm8 * 0x0101_0101,
        }
    } else {
        (0x80 | (imm12 & 0x7F)).rotate_right(imm12 >> 7)
    }
}

/// Whether a half-word starts a 32-bit Thumb instruction.
pub fn is_thumb32(hw: u16) -> bool {
    hw & 0xF800 >= 0xE800
}

/// Decodes the prologue in `code` (located at the function start `start`) up to `pc`.
pub fn analyze_prologue(code: &[u8], start: u32, pc: u32) -> Prologue {
    let mut p = Prologue::default();
    let half = |off: usize| code.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32);
    let push = |p: &mut Prologue, mask: u32| {
        let regs: Vec<usize> = (0..16).filter(|r| mask & (1 << r) != 0).collect();
        let n = regs.len() as u32;
        // Lowest register at the lowest address.
        for (i, r) in regs.into_iter().enumerate() {
            p.saved.push((r, p.stack + 4 * n - 4 * i as u32));
        }
        p.stack += 4 * n;
    };
    let mut off = 0usize;
    while start.wrapping_add(off as u32) < pc && (off as u32) < MAX_PROLOGUE {
        let Some(hw) = half(off) else { break };
        if !is_thumb32(hw as u16) {
            if hw & 0xFE00 == 0xB400 {
                // push {r0-r7, lr}
                push(&mut p, (hw & 0xFF) | if hw & 0x100 != 0 { 1 << R_LR } else { 0 });
            } else if hw & 0xFF80 == 0xB080 {
                // sub sp, #imm7 * 4
                p.stack += (hw & 0x7F) * 4;
            }
            off += 2;
            continue;
        }
        let Some(hw2) = half(off + 2) else { break };
        let imm12 = (hw >> 10 & 1) << 11 | (hw2 >> 12 & 7) << 8 | (hw2 & 0xFF);
        if hw == 0xE92D {
            // push.w {reglist}
            push(&mut p, hw2 & 0x5FFF);
        } else if hw == 0xF84D && hw2 & 0x0FFF == 0x0D04 {
            // str.w rt, [sp, #-4]!
            push(&mut p, 1 << (hw2 >> 12));
        } else if hw & 0xFBEF == 0xF1AD && hw2 & 0x8F00 == 0x0D00 {
            // sub.w sp, sp, #const
            p.stack += thumb_expand_imm(imm12);
        } else if hw & 0xFBFF == 0xF2AD && hw2 & 0x8F00 == 0x0D00 {
            // subw sp, sp, #imm12
            p.stack += imm12;
        }
        off += 4;
    }
    p
}

fn read32(read: &mut dyn FnMut(u32, usize) -> Option<Vec<u8>>, addr: u32) -> Option<u32> {
    let b = read(addr, 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The interrupted context of an exception frame: `exc_return` selects the stack, `msp` is
/// the handler's SP at the exception return.
fn exception_caller(cur: &Frame, exc_return: u32, msp: u32, read: &mut dyn FnMut(u32, usize) -> Option<Vec<u8>>) -> Option<Frame> {
    let thread_psp = exc_return & 4 != 0;
    let sp = if thread_psp { cur.regs[R_PSP]? } else { msp };
    let b = read(sp, 32)?;
    let w = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
    let mut regs = [None; 32];
    for (slot, i) in [(0, 0), (1, 1), (2, 2), (3, 3), (12, 4), (R_LR, 5)] {
        regs[slot] = Some(w(i));
    }
    // r4-r11 are restored by the handler's own epilogue: the handler frame's values.
    for r in 4..12 {
        regs[r] = cur.regs[r];
    }
    let xpsr = w(7);
    let caller_sp = sp + 32 + if xpsr & (1 << 9) != 0 { 4 } else { 0 };
    regs[R_SP] = Some(caller_sp);
    regs[R_MSP] = if thread_psp { Some(msp) } else { Some(caller_sp) };
    regs[R_PSP] = if thread_psp { Some(caller_sp) } else { cur.regs[R_PSP] };
    let pc = w(6) & !1;
    regs[R_PC] = Some(pc);
    Some(Frame { pc, pcxi: caller_sp, psw: Some(xpsr), regs, arm: true })
}

/// Unwinds from the top frame. `func_start` returns the start of the function containing
/// an address, `read` reads target memory.
pub fn unwind(
    top: Frame,
    max_frames: usize,
    func_start: &dyn Fn(u32) -> Option<u32>,
    read: &mut dyn FnMut(u32, usize) -> Option<Vec<u8>>,
) -> Vec<Frame> {
    let mut frames = vec![top];
    while frames.len() < max_frames {
        let cur = frames.last().unwrap().clone();
        let top_frame = frames.len() == 1;
        // A caller frame's PC is a return address: the call is the instruction before it.
        let look = if top_frame { cur.pc } else { cur.pc.wrapping_sub(1) };
        let Some(start) = func_start(look) else { break };
        let Some(sp) = cur.regs[R_SP] else { break };
        let len = (cur.pc.wrapping_sub(start)).min(MAX_PROLOGUE) + 4;
        let Some(code) = read(start, len as usize) else { break };
        let p = analyze_prologue(&code, start, cur.pc);
        let entry_sp = sp.wrapping_add(p.stack);

        let mut regs = [None; 32];
        for r in 4..12 {
            regs[r] = cur.regs[r];
        }
        for &(r, off) in &p.saved {
            if r < 16 && r != R_SP {
                regs[r] = read32(read, entry_sp.wrapping_sub(off));
            }
        }
        // LR is only known in the top frame and in interrupted frames (from an exception frame,
        // marked by their stacked xPSR).
        let lr_known = top_frame || cur.psw.is_some();
        let ra = match p.saved.iter().any(|(r, _)| *r == R_LR) {
            true => regs[R_LR],
            // A function that has not saved LR (yet) returns to LR.
            false if lr_known => cur.regs[R_LR],
            false => None,
        };
        let Some(ra) = ra else { break };

        let caller = if ra >= EXC_RETURN_MIN {
            match exception_caller(&cur, ra, entry_sp, read) {
                Some(f) => f,
                None => break,
            }
        } else {
            let pc = ra & !1;
            regs[R_SP] = Some(entry_sp);
            regs[R_PC] = Some(pc);
            regs[R_MSP] = cur.regs[R_MSP];
            regs[R_PSP] = cur.regs[R_PSP];
            Frame { pc, pcxi: entry_sp, psw: None, regs, arm: true }
        };
        if caller.pc == 0 || caller.regs[R_SP].unwrap_or(0) < sp && ra < EXC_RETURN_MIN {
            break;
        }
        if frames.iter().any(|f| f.pc == caller.pc && f.regs[R_SP] == caller.regs[R_SP]) {
            break;
        }
        frames.push(caller);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn thumb(words: &[u16]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn decodes_prologue_instructions() {
        // push {r4, r5, r7, lr}; sub sp, #16; mov r4, r0
        let code = thumb(&[0xB5B0, 0xB084, 0x4604]);
        let p = analyze_prologue(&code, 0x1000, 0x1006);
        assert_eq!(p.stack, 32);
        assert_eq!(p.saved, vec![(4, 16), (5, 12), (7, 8), (R_LR, 4)]);
        // Only the push has executed at 0x1002.
        assert_eq!(analyze_prologue(&code, 0x1000, 0x1002).stack, 16);
        // push.w {r4-r11, lr}; sub.w sp, sp, #0x100; subw sp, sp, #0x204
        let code = thumb(&[0xE92D, 0x4FF0, 0xF5AD, 0x7D80, 0xF2AD, 0x2D04]);
        let p = analyze_prologue(&code, 0x2000, 0x200C);
        assert_eq!(p.stack, 36 + 0x100 + 0x204);
        assert_eq!(p.saved.last(), Some(&(R_LR, 4)));
        assert_eq!(p.saved.first(), Some(&(4, 36)));
        // str.w lr, [sp, #-4]!
        let code = thumb(&[0xF84D, 0xED04]);
        assert_eq!(analyze_prologue(&code, 0, 4).saved, vec![(R_LR, 4)]);
        assert_eq!(thumb_expand_imm(0xF80), 0x100);
        assert_eq!(thumb_expand_imm(0x1AB), 0x00AB_00AB);
    }

    struct Mem(HashMap<u32, u8>);
    impl Mem {
        fn put(&mut self, addr: u32, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                self.0.insert(addr + i as u32, *b);
            }
        }
        fn put32(&mut self, addr: u32, v: u32) {
            self.put(addr, &v.to_le_bytes());
        }
        fn read(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
            (0..len as u32).map(|i| self.0.get(&(addr + i)).copied()).collect()
        }
    }

    #[test]
    fn unwinds_calls_and_an_exception_frame() {
        let mut m = Mem(HashMap::new());
        // main at 0x8000: push {r7, lr} - calls isr-interrupted task
        m.put(0x8000, &thumb(&[0xB580, 0xBF00, 0xBF00, 0xBF00]));
        // task at 0x8100: push {r4, lr}; sub sp, #8 - interrupted at 0x8108
        m.put(0x8100, &thumb(&[0xB510, 0xB082, 0xBF00, 0xBF00, 0xBF00, 0xBF00]));
        // handler at 0x8200: push {r4, lr} - calls leaf
        m.put(0x8200, &thumb(&[0xB510, 0xBF00, 0xBF00]));
        // leaf at 0x8300: no prologue
        m.put(0x8300, &thumb(&[0xBF00, 0xBF00]));
        let funcs = [0x8000u32, 0x8100, 0x8200, 0x8300];
        let func_start = |a: u32| funcs.iter().rev().find(|&&f| a >= f && a < f + 0x100).copied();

        // Task stack (PSP): task frame r4, lr at entry-8, entry-4; task entry SP = 0x2000_1000.
        m.put32(0x2000_0FF8, 0x1111_1111); // r4
        m.put32(0x2000_0FFC, 0x8003); // lr -> main + 2 (Thumb)
        // Exception frame on PSP below the task's SP (entry SP - push 8 - sub 8).
        let task_sp = 0x2000_1000 - 16;
        let ef = [0u32, 1, 2, 3, 12, 0x8105, 0x8108, 0x0100_0000];
        for (i, v) in ef.iter().enumerate() {
            m.put32(task_sp - 32 + 4 * i as u32, *v);
        }
        let psp = task_sp - 32;
        // Handler on MSP: entry SP 0x2001_0000, saved r4 and lr = EXC_RETURN (thread, PSP).
        m.put32(0x2000_FFF8, 0x4444_4444);
        m.put32(0x2000_FFFC, 0xFFFF_FFFD);
        let msp = 0x2000_FFF8;

        let mut regs = [None; 32];
        regs[R_SP] = Some(msp);
        regs[R_LR] = Some(0x8203);
        regs[R_PC] = Some(0x8300);
        regs[R_MSP] = Some(msp);
        regs[R_PSP] = Some(psp);
        let top = Frame { pc: 0x8300, pcxi: 0, psw: None, regs, arm: true };
        let frames = unwind(top, 16, &func_start, &mut |a, l| m.read(a, l));
        let pcs: Vec<u32> = frames.iter().map(|f| f.pc).collect();
        assert_eq!(pcs, vec![0x8300, 0x8202, 0x8108, 0x8002], "{frames:#x?}");
        assert_eq!(frames[1].regs[R_SP], Some(msp));
        assert_eq!(frames[2].regs[R_SP], Some(task_sp));
        assert_eq!(frames[2].regs[0], Some(0));
        assert_eq!(frames[3].regs[R_SP], Some(0x2000_1000));
        assert_eq!(frames[3].regs[4], Some(0x1111_1111));
    }
}
