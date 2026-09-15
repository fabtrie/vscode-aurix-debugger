//! Call stack of a halted core: registers plus the CSA chain.

use aurix_core::arch::{REG_PC, REG_PCXI, REG_PSW};
use aurix_core::unwind::{self, Frame};
use aurix_core::ServerHandle;

pub const MAX_FRAMES: usize = 64;

pub fn unwind_core(h: &ServerHandle, idx: usize) -> Result<Vec<Frame>, String> {
    h.call(move |cx| {
        cx.with(|c| -> Result<Vec<Frame>, String> {
            let (mut io, e) = c.target_io(idx).map_err(|e| e.to_string())?;
            let mut idxs: Vec<usize> = (0..32).collect();
            idxs.extend([REG_PC, REG_PCXI, REG_PSW]);
            let v = e.core.read_regs(&mut io, &idxs).map_err(|e| e.to_string())?;
            let mut regs = [None; 32];
            for (i, r) in regs.iter_mut().enumerate() {
                *r = Some(v[i]);
            }
            let top = Frame { pc: v[32], pcxi: v[33], psw: Some(v[34]), regs };
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
