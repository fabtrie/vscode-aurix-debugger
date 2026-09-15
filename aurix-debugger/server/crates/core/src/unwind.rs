//! TriCore call stack unwinding through the context save areas (CSA).
//!
//! A CALL saves the caller's upper context in a CSA linked from PCXI and
//! loads the return address into A11; an interrupt does the same with the
//! interrupted PC. So for frame N the caller's PC is A11(N) and the caller's
//! non-volatile registers are in the upper CSA at PCXI(N). Lower contexts
//! (bisr/svlcx) in the chain are restored and followed without a new frame.
//! The DWARF CFI of GHS marks these registers `same_value`, which does not
//! describe CSA saving, so it is not used.

/// PCXI.UL (upper context) bit - bit 20 on TC1.6.2P (verified on TC377) and TC1.8.
pub const PCXI_UL: u32 = 1 << 20;
pub const PCXI_LINK_MASK: u32 = 0x000F_FFFF;

/// Register numbering: D0..D15 = 0..15, A0..A15 = 16..31.
pub const A10: usize = 26;
pub const A11: usize = 27;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pc: u32,
    pub pcxi: u32,
    pub psw: Option<u32>,
    pub regs: [Option<u32>; 32],
}

impl Frame {
    pub fn sp(&self) -> Option<u32> {
        self.regs[A10]
    }
    pub fn ra(&self) -> Option<u32> {
        self.regs[A11]
    }
}

/// Effective address of the CSA a PCXI/FCX link word points to.
pub fn csa_address(link: u32) -> Option<u32> {
    let l = link & PCXI_LINK_MASK;
    if l == 0 {
        None
    } else {
        Some(((l >> 16) << 28) | ((l & 0xFFFF) << 6))
    }
}

fn plausible_code(addr: u32) -> bool {
    addr != 0 && addr != 0xFFFF_FFFF && addr & 1 == 0
}

/// Walks the CSA chain starting at `top`. `read_csa` returns the 16 words of
/// the CSA at an address.
pub fn unwind(top: Frame, max_frames: usize, read_csa: &mut dyn FnMut(u32) -> Option<[u32; 16]>) -> Vec<Frame> {
    let mut frames = vec![top];
    while frames.len() < max_frames {
        let cur = frames.last().unwrap().clone();
        let mut regs = cur.regs;
        let mut pcxi = cur.pcxi;

        // Lower contexts on top of the chain belong to the current frame.
        let mut guard = 0;
        while pcxi & PCXI_LINK_MASK != 0 && pcxi & PCXI_UL == 0 && guard < 4 {
            let Some(w) = csa_address(pcxi).and_then(&mut *read_csa) else { return frames };
            // PCXI, A11, A2, A3, D0, D1, D2, D3, A4, A5, A6, A7, D4, D5, D6, D7
            regs[A11] = Some(w[1]);
            regs[18] = Some(w[2]);
            regs[19] = Some(w[3]);
            for k in 0..4 {
                regs[k] = Some(w[4 + k]);
                regs[20 + k] = Some(w[8 + k]);
                regs[4 + k] = Some(w[12 + k]);
            }
            pcxi = w[0];
            guard += 1;
        }

        let Some(ra) = regs[A11] else { break };
        if pcxi & PCXI_LINK_MASK == 0 || !plausible_code(ra) {
            break;
        }
        let Some(w) = csa_address(pcxi).and_then(&mut *read_csa) else { break };
        // PCXI, PSW, A10, A11, D8, D9, D10, D11, A12, A13, A14, A15, D12, D13, D14, D15
        let mut caller = [None; 32];
        caller[A10] = Some(w[2]);
        caller[A11] = Some(w[3]);
        for k in 0..4 {
            caller[8 + k] = Some(w[4 + k]);
            caller[28 + k] = Some(w[8 + k]);
            caller[12 + k] = Some(w[12 + k]);
        }
        // A0, A1, A8, A9 are global pointers, not saved.
        for a in [16usize, 17, 24, 25] {
            caller[a] = regs[a];
        }
        let f = Frame { pc: ra, pcxi: w[0], psw: Some(w[1]), regs: caller };
        if frames.iter().any(|x| x.pc == f.pc && x.pcxi == f.pcxi) {
            break;
        }
        frames.push(f);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn csa_link_decoding() {
        // Values read on a TC377: PCXI 0x00370264 -> CSA pool of CPU0.
        assert_eq!(csa_address(0x0037_0264), Some(0x7000_9900));
        assert_ne!(0x0037_0264 & PCXI_UL, 0);
        assert_eq!(csa_address(0x0030_0000), None);
    }

    #[test]
    fn walks_upper_and_lower_contexts() {
        let mut mem: HashMap<u32, [u32; 16]> = HashMap::new();
        // frame0 (func C) PCXI -> lower context (svlcx) -> upper context of B -> upper of A -> end
        let lower_link = 0x0007_0010; // UL=0
        let upper_b = 0x0017_0020; // UL=1
        let upper_a = 0x0017_0030;
        let mut lower = [0u32; 16];
        lower[0] = upper_b;
        lower[1] = 0x8000_2004; // A11 in C before svlcx: return into B
        mem.insert(csa_address(lower_link).unwrap(), lower);
        let mut ub = [0u32; 16];
        ub[0] = upper_a;
        ub[2] = 0x7000_1000; // B's SP
        ub[3] = 0x8000_1008; // B's A11: return into A
        mem.insert(csa_address(upper_b).unwrap(), ub);
        let mut ua = [0u32; 16];
        ua[0] = 0; // end of chain
        ua[2] = 0x7000_2000;
        ua[3] = 0x8000_0000 + 0x10;
        mem.insert(csa_address(upper_a).unwrap(), ua);

        let mut regs = [None; 32];
        regs[A11] = Some(0xDEAD_BEEE); // overwritten by the lower context
        let top = Frame { pc: 0x8000_3000, pcxi: lower_link, psw: None, regs };
        let frames = unwind(top, 16, &mut |a| mem.get(&a).copied());
        let pcs: Vec<u32> = frames.iter().map(|f| f.pc).collect();
        assert_eq!(pcs, vec![0x8000_3000, 0x8000_2004, 0x8000_1008]);
        assert_eq!(frames[1].sp(), Some(0x7000_1000));
        assert_eq!(frames[2].sp(), Some(0x7000_2000));
    }
}
