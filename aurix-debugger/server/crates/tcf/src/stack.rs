//! Call stack of a halted core: registers plus the CSA chain (TriCore) or the
//! decoded function prologues (Cortex-M, the HSM).

use aurix_core::arch::{CM_REGS, MCS_REG_CTRL, MCS_REG_PC, MCS_REG_R7, REG_PC, REG_PCXI, REG_PSW};
use aurix_core::unwind::{self, Frame};
use aurix_core::{ops, unwind_arm};

use crate::contexts::find_core;
use crate::Shared;

pub const MAX_FRAMES: usize = 64;

pub fn unwind_core(sh: &Shared, idx: usize) -> Result<Vec<Frame>, String> {
    let core = find_core(&sh.handle, &crate::contexts::core_id_of(&sh.handle, idx)).ok_or("invalid core")?;
    if core.arch.name == "cortex_m" {
        return unwind_cortex_m(sh, idx, &core.id);
    }
    if core.arch.name == "gtm_mcs" {
        return unwind_mcs(sh, idx);
    }
    sh.handle
        .call(move |cx| {
            cx.with(|c| -> Result<Vec<Frame>, String> {
                let (mut io, e) = c.target_io(idx).map_err(|e| e.to_string())?;
                let mut idxs: Vec<usize> = (0..32).collect();
                idxs.extend([REG_PC, REG_PCXI, REG_PSW]);
                let v = e.core.read_regs(&mut io, &idxs).map_err(|e| e.to_string())?;
                let mut regs = [None; 32];
                for (i, r) in regs.iter_mut().enumerate() {
                    *r = Some(v[i]);
                }
                let top = Frame { pc: v[32], pcxi: v[33], psw: Some(v[34]), regs, arm: false };
                let mut read = |addr: u32| -> Option<[u32; 16]> {
                    let b = io.read(addr, 64).ok()?;
                    let mut w = [0u32; 16];
                    for (k, x) in w.iter_mut().enumerate() {
                        *x = u32::from_le_bytes(b[4 * k..4 * k + 4].try_into().unwrap());
                    }
                    Some(w)
                };
                Ok(unwind::unwind(top, MAX_FRAMES, &mut read))
            })
        })
        .unwrap_or_else(|| Err("server stopped".into()))
}

/// GTM MCS channel: the current instruction, plus the caller when the top of the stack (R7) holds
/// a return address, i.e. an address right after a CALL. PUSH also uses the stack, so deeper
/// frames cannot be told apart from data.
fn unwind_mcs(sh: &Shared, idx: usize) -> Result<Vec<Frame>, String> {
    sh.handle
        .call(move |cx| {
            let v = cx
                .with(|c| {
                    let (mut io, e) = c.target_io(idx)?;
                    e.core.read_regs(&mut io, &[MCS_REG_PC, MCS_REG_R7, MCS_REG_CTRL])
                })
                .map_err(|e| e.to_string())?;
            let (pc, sp, sp_cnt) = (v[0], v[1] & 0x7FFC, (v[2] >> 16) & 7);
            let frame = |pc: u32| {
                let mut regs = [None; 32];
                regs[7] = Some(sp);
                Frame { pc, pcxi: 0, psw: None, regs, arm: false }
            };
            let mut frames = vec![frame(pc)];
            if sp_cnt > 0 {
                let read = |a: u32| ops::read_memory_as(cx, idx, a, 4).ok().map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                let ret = read(sp).filter(|r| *r >= 4 && r & 3 == 0 && *r < aurix_core::gtm_mcs::RAM_SIZE);
                let after_call = ret
                    .and_then(|r| read(r - 4))
                    .and_then(isa::mcs::decode)
                    .is_some_and(|op| matches!(op.flow, isa::mcs::Flow::Call | isa::mcs::Flow::CallR6));
                if let (Some(r), true) = (ret, after_call) {
                    frames.push(frame(r));
                }
            }
            Ok(frames)
        })
        .unwrap_or_else(|| Err("server stopped".into()))
}

fn unwind_cortex_m(sh: &Shared, idx: usize, ctx: &str) -> Result<Vec<Frame>, String> {
    let syms = sh.symbols_for(ctx);
    let func_start = move |addr: u32| syms.as_ref()?.function_at(addr).map(|f| f.low & !1);
    sh.handle
        .call(move |cx| {
            let names = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr", "pc", "xpsr", "msp", "psp"];
            let idxs: Vec<usize> = names.iter().map(|n| CM_REGS.iter().position(|r| r.name == *n).unwrap()).collect();
            let v = cx
                .with(|c| {
                    let (mut io, e) = c.target_io(idx)?;
                    e.core.read_regs(&mut io, &idxs)
                })
                .map_err(|e| e.to_string())?;
            let mut regs = [None; 32];
            for (i, r) in regs.iter_mut().enumerate().take(16) {
                *r = Some(v[i]);
            }
            regs[unwind_arm::R_MSP] = Some(v[17]);
            regs[unwind_arm::R_PSP] = Some(v[18]);
            let pc = v[15] & !1;
            let top = Frame { pc, pcxi: v[13], psw: Some(v[16]), regs, arm: true };
            let mut read = |addr: u32, len: usize| ops::read_memory_as(cx, idx, addr, len).ok();
            Ok(unwind_arm::unwind(top, MAX_FRAMES, &func_start, &mut read))
        })
        .unwrap_or_else(|| Err("server stopped".into()))
}
