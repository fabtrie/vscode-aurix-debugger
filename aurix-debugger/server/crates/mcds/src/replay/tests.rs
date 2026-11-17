use std::collections::HashMap;

use super::*;
use crate::flow::Step;

struct Code(HashMap<u32, Decoded>);

impl Disasm for Code {
    fn disasm(&mut self, a: u32) -> Option<Decoded> {
        self.0.get(&a).cloned()
    }
}

fn code(list: &[(u32, u8, &str, &str)]) -> Code {
    Code(list.iter().map(|&(a, size, m, o)| (a, Decoded { size, mnemonic: m.into(), ops: o.into() })).collect())
}

fn steps(addrs: &[u32]) -> Vec<Step> {
    addrs.iter().map(|&addr| Step::Insn { addr, time: None }).collect()
}

fn reg(r: &Replay, row: Option<usize>, name: &str) -> RegOut {
    r.regs(row).into_iter().find(|x| x.name == name).unwrap()
}

fn mem_of(list: &[(u32, &[u8])]) -> HashMap<u32, u8> {
    let mut m = HashMap::new();
    for &(a, bytes) in list {
        for (i, b) in bytes.iter().enumerate() {
            m.insert(a + i as u32, *b);
        }
    }
    m
}

fn words(ws: &[u32]) -> Vec<u8> {
    ws.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[test]
fn undo_from_the_halt() {
    let c = &mut code(&[(0x100, 2, "add", "d8, #1"), (0x102, 2, "add", "d8, #1"), (0x104, 2, "mov", "d8, #5"), (0x106, 2, "add", "d2, d8")]);
    let mut regs = vec![0u32; 42];
    regs[8] = 5;
    regs[2] = 0x20;
    regs[36] = 0x108;
    let r = Replay::build(&steps(&[0x100, 0x102, 0x104, 0x106]), c, Some(&HaltRegs(regs)), &mut |_, _| None, 0);
    // d8 before the mov: overwritten, unknown; d2 before the add: 0x20 - 5.
    assert!(!reg(&r, Some(1), "d8").is_known());
    assert_eq!(reg(&r, Some(2), "d2").value, 0x1B);
    assert_eq!(reg(&r, None, "d2").how, Some(How::Inverse));
    assert_eq!(reg(&r, Some(3), "d2").how, Some(How::Halt));
    assert_eq!(r.written(3).iter().map(|x| x.name).collect::<Vec<_>>(), vec!["d2", "psw"]);
}

#[test]
fn no_anchor_keeps_constants_only() {
    let c = &mut code(&[(0x100, 4, "mov", "d3, #9"), (0x104, 2, "add", "d3, #1")]);
    let r = Replay::build(&steps(&[0x100, 0x104]), c, None, &mut |_, _| None, 0);
    assert_eq!(reg(&r, Some(1), "d3").value, 10);
    assert!(!reg(&r, Some(1), "d4").is_known());
}

#[test]
fn branch_outcomes_are_facts() {
    let c = &mut code(&[(0x100, 2, "jz", "d4, #0x110"), (0x110, 2, "jne", "d5, #3, #0x200"), (0x112, 2, "nop", "")]);
    let r = Replay::build(&steps(&[0x100, 0x110, 0x112]), c, None, &mut |_, _| None, 0);
    assert_eq!(reg(&r, None, "d4").value, 0);
    assert_eq!(reg(&r, None, "d4").how, Some(How::Fact));
    assert_eq!(reg(&r, None, "d5").value, 3);
}

#[test]
fn memory_contradicting_a_branch_is_rejected() {
    // ld.w d15 from 0x70000010 (memory at the halt: 0xFFFFFFFF), then jne d15,#-1 taken: the
    // value loaded was not 0xFFFFFFFF, memory changed after the load.
    let c = &mut code(&[(0x100, 4, "ld.w", "d15, [a2]#0x10"), (0x104, 4, "jne", "d15, #-1, #0x200"), (0x200, 2, "mov", "d15, #0")]);
    let mut regs = vec![0u32; 42];
    regs[18] = 0x7000_0000;
    regs[36] = 0x202;
    let mut read = |a: u32, n: usize| Some((0..n).map(|i| if (0x7000_0010..0x7000_0014).contains(&(a + i as u32)) { 0xFF } else { 0 }).collect());
    let r = Replay::build(&steps(&[0x100, 0x104, 0x200]), c, Some(&HaltRegs(regs)), &mut read, 0);
    assert!(!reg(&r, Some(0), "d15").is_known(), "{:?}", reg(&r, Some(0), "d15"));
    // Without the branch it would have been taken from memory.
    let c2 = &mut code(&[(0x100, 4, "ld.w", "d15, [a2]#0x10"), (0x104, 2, "mov", "d15, #0")]);
    let mut regs2 = vec![0u32; 42];
    regs2[18] = 0x7000_0000;
    regs2[36] = 0x106;
    let r2 = Replay::build(&steps(&[0x100, 0x104]), c2, Some(&HaltRegs(regs2)), &mut read, 0);
    assert_eq!(reg(&r2, Some(0), "d15").value, 0xFFFF_FFFF);
}

#[test]
fn interrupt_entry_is_detected() {
    // A non-branch followed by an instruction elsewhere: an interrupt came in between.
    let c = &mut code(&[(0x100, 2, "mov", "d1, #1"), (0x800, 4, "svlcx", "")]);
    let r = Replay::build(&steps(&[0x100, 0x800]), c, None, &mut |_, _| None, 0);
    assert!(matches!(r.rows[1], Row::Entry { resume: Some(0x102), vector: 0x800, .. }));
    assert_eq!(reg(&r, Some(1), "a11").value, 0x102);
    assert_eq!(r.stats.entries, 1);
}

/// A loop summing bytes, a call storing a message into a FIFO, the return
/// through the CSA, and a load of the count the callee stored.
#[test]
fn loop_call_store_and_return() {
    let c = &mut code(&[
        (0x8000_1000, 4, "addsc.a", "a15, a12, d8, #0"),
        (0x8000_1004, 4, "ld.bu", "d15, [a15]#4"),
        (0x8000_1008, 2, "add", "d9, d15"),
        (0x8000_100a, 2, "add", "d8, #1"),
        (0x8000_100c, 4, "ld.bu", "d15, [a12]#3"),
        (0x8000_1010, 4, "jlt.u", "d8, d15, #0x80001000"),
        (0x8000_1014, 4, "movh.a", "a4, #0x7000"),
        (0x8000_1018, 4, "lea", "a4, [a4]#0x1a0"),
        (0x8000_101c, 2, "mov.aa", "a5, a12"),
        (0x8000_101e, 4, "call", "#0x80002000"),
        (0x8000_1022, 2, "mov", "d10, d2"),
        (0x8000_1024, 4, "movh.a", "a15, #0x7000"),
        (0x8000_1028, 4, "ld.w", "d15, [a15]#0x1a8"),
        (0x8000_102c, 4, "mov", "d0, #0xa"),
        (0x8000_1030, 4, "jge.u", "d0, d15, #0x80001050"),
        (0x8000_2000, 2, "sub.a", "sp, #8"),
        (0x8000_2002, 4, "ld.w", "d15, [a4]#8"),
        (0x8000_2006, 4, "mov", "d0, #0x10"),
        (0x8000_200a, 4, "jge.u", "d15, d0, #0x80002040"),
        (0x8000_200e, 4, "ld.w", "d1, [a4]#4"),
        (0x8000_2012, 2, "add", "d1, d15"),
        (0x8000_2014, 4, "and", "d1, d1, #0xf"),
        (0x8000_2018, 4, "sh", "d2, d1, #4"),
        (0x8000_201c, 4, "lea", "a2, [a4]#0x10"),
        (0x8000_2020, 4, "addsc.a", "a2, a2, d2, #0"),
        (0x8000_2024, 4, "ld.d", "e4, [a5]#0"),
        (0x8000_2028, 4, "st.d", "[a2]#0, e4"),
        (0x8000_202c, 4, "ld.d", "e4, [a5]#8"),
        (0x8000_2030, 4, "st.d", "[a2]#8, e4"),
        (0x8000_2034, 2, "add", "d15, #1"),
        (0x8000_2036, 4, "st.w", "[a4]#8, d15"),
        (0x8000_203a, 2, "mov", "d2, #1"),
        (0x8000_203c, 2, "ret", ""),
    ]);
    let lp: Vec<u32> = vec![0x8000_1000, 0x8000_1004, 0x8000_1008, 0x8000_100a, 0x8000_100c, 0x8000_1010];
    let mut addrs = lp.clone();
    addrs.extend(&lp);
    addrs.extend([0x8000_1014, 0x8000_1018, 0x8000_101c, 0x8000_101e]);
    addrs.extend((0..18).map(|i| [0x2000u32, 0x2002, 0x2006, 0x200a, 0x200e, 0x2012, 0x2014, 0x2018, 0x201c, 0x2020, 0x2024, 0x2028, 0x202c, 0x2030, 0x2034, 0x2036, 0x203a, 0x203c][i] | 0x8000_0000));
    addrs.extend([0x8000_1022, 0x8000_1024, 0x8000_1028, 0x8000_102c, 0x8000_1030]);

    let (f1, f2, p0) = (0x0007_0A10u32, 0x0007_0A0Fu32, 0x0037_0A11u32);
    let c1 = 0x7002_8400u32;
    let mut regs = vec![0u32; 42];
    let set = |regs: &mut Vec<u32>, l: &[(usize, u32)]| l.iter().for_each(|&(i, v)| regs[i] = v);
    set(&mut regs, &[(0, 10), (1, 9), (2, 1), (3, 0x41), (4, 0x3E3C_6655), (5, 0x0001_F3A0), (7, 0x8000_0000), (8, 8), (9, 0x1DF), (10, 1), (11, 3), (12, 0xFFFF), (14, 1), (15, 0x0C)]);
    set(&mut regs, &[(16, 0x7000_8000), (17, 0x8000_8000), (18, 0x7000_0240), (19, 0x8001_3000), (20, 0x7000_01A0), (21, 0x7000_1234), (22, 0x7000_1300), (26, 0x7001_9F40), (27, 0x8000_4F1A), (28, 0x7000_1234), (29, 0x7000_0400), (31, 0x7000_0000)]);
    set(&mut regs, &[(33, f1), (34, p0), (35, 0x0000_0980), (36, 0x8000_1034), (37, 0x8000), (38, 0x7001_9000)]);
    let msg = [0x23, 0x01, 0x00, 0x08, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x3c, 0x3e, 0xA0, 0xF3, 0x01, 0x00];
    // The CSA after the return: word 0 is the free-list link the return wrote.
    let csa = words(&[f2, 0x0000_0980, 0x7001_9F40, 0x8000_4F1A, 8, 0x1DF, 0x55, 3, 0x7000_1234, 0x7000_0400, 0, 0x7000_123B, 0xFFFF, 0, 1, 8]);
    let memory = mem_of(&[(0x7000_1234, &msg), (0x7000_01A0, &words(&[0, 0x0E, 0x0C, 0])), (0x7000_0240, &msg), (c1, &csa)]);
    let mut read = |a: u32, n: usize| Some((0..n).map(|i| memory.get(&(a + i as u32)).copied().unwrap_or(0)).collect());
    let r = Replay::build(&steps(&addrs), c, Some(&HaltRegs(regs)), &mut read, 0);
    assert_eq!(r.stats.unmodelled, vec![]);
    assert_eq!(r.stats.conflicts, 0);
    let row_of = |a: u32, nth: usize| r.rows.iter().enumerate().filter(|(_, x)| matches!(x, Row::Insn { addr, .. } if *addr == a)).nth(nth).unwrap().0;

    // Loop counter undone from the halt. The bytes the loop sums are exact: the callee loads the
    // same bytes later with ld.d into d4, which still holds them at the halt.
    assert_eq!(reg(&r, None, "d8").value, 6);
    assert_eq!(reg(&r, None, "d8").mark, Some(Mark::Exact));
    let d9 = reg(&r, None, "d9");
    assert_eq!((d9.value, d9.mark), (0x165, Some(Mark::Exact)));
    let b = reg(&r, Some(1), "d15");
    assert_eq!((b.value, b.mark), (0x3C, Some(Mark::Exact)));
    let ldd = row_of(0x8000_202c, 0) as u32;
    assert_eq!(b.how, Some(How::Mem(MemFrom::Load(ldd), 0x7000_123E)));
    // The head index is only in memory at the halt.
    let head = reg(&r, Some(row_of(0x8000_200e, 0)), "d1");
    assert_eq!((head.value, head.mark), (0x0E, Some(Mark::Mem)));
    assert_eq!(head.how, Some(How::Mem(MemFrom::Halt, 0x7000_01A4)));
    // The second length load is exact: the call saved d15 in the CSA.
    let len2 = reg(&r, Some(row_of(0x8000_100c, 1)), "d15");
    assert_eq!((len2.value, len2.mark), (8, Some(Mark::Exact)));
    // d10 before the call: restored by the return from the CSA, then overwritten.
    let d10 = reg(&r, None, "d10");
    assert_eq!((d10.value, d10.mark), (0x55, Some(Mark::Exact)));
    // The count load in the callee: the caller loads what the store wrote (exact, its d15 is
    // the halt value), so the loaded count is the stored one minus 1.
    let cnt = reg(&r, Some(row_of(0x8000_2002, 0)), "d15");
    assert_eq!((cnt.value, cnt.mark), (0x0B, Some(Mark::Exact)));
    // PCXI and FCX before the call.
    let pcxi = reg(&r, None, "pcxi");
    assert_eq!((pcxi.value, pcxi.known), (p0, u32::MAX));
    assert_eq!(reg(&r, None, "fcx").value, f1);
    // Inside the callee: PCXI links the CSA, A11 the return address, SP moved.
    let call = row_of(0x8000_101e, 0);
    assert_eq!(reg(&r, Some(call), "a11").value, 0x8000_1022);
    assert_eq!(reg(&r, Some(call), "pcxi").value & 0xFFFFF, f1);
    assert_eq!(reg(&r, Some(call + 1), "a10").value, 0x7001_9F38);
    // a4 before the window: overwritten (movh.a) and not saved anywhere.
    assert!(!reg(&r, None, "a4").is_known());
    // Effects of the count store: address and value.
    let st = r.accesses(row_of(0x8000_2036, 0));
    assert_eq!(st, vec![AccessOut { store: true, addr: Some(0x7000_01A8), len: 4, value: Some(vec![0x0C]), context: false }]);
    // An explanation names the row.
    let e = r.explain(&cnt, &|row| format!("#{}", row + 1));
    assert!(e.contains("Undone through #"), "{e}");
}

#[test]
fn entry_state_spreads_forwards() {
    // From the halt alone the store address and d8 before the mov are lost; with the state the
    // core started from, every value is exact, including the stored address.
    let c = &mut code(&[(0x100, 2, "add", "d8, #1"), (0x102, 4, "st.w", "[a4]#0, d8"), (0x106, 2, "mov", "d8, #5"), (0x108, 2, "mov.a", "a4, d8")]);
    let mut entry = vec![0u32; 42];
    entry[8] = 0x41;
    entry[20] = 0x7000_0100;
    entry[36] = 0x100;
    let mut halt = entry.clone();
    halt[8] = 5;
    halt[20] = 5;
    halt[36] = 0x10A;
    let s = steps(&[0x100, 0x102, 0x106, 0x108]);
    let only_halt = Replay::build(&s, c, Some(&HaltRegs(halt.clone())), &mut |_, _| None, 0);
    assert!(!reg(&only_halt, Some(0), "d8").is_known());
    assert_eq!(only_halt.accesses(1)[0].addr, None);
    let both = Replay::build_between(&s, c, Some(&HaltRegs(entry.clone())), Some(&HaltRegs(halt)), &mut |_, _| None, 0);
    assert_eq!(reg(&both, Some(0), "d8").value, 0x42);
    assert_eq!(reg(&both, None, "d8").how, Some(How::Entry));
    let st = &both.accesses(1)[0];
    assert!(st.store);
    assert_eq!(st.addr, Some(0x7000_0100));
    assert_eq!(st.value, Some(vec![0x42]));
    // An entry state that does not start at the first row is not used.
    entry[36] = 0x200;
    let other = Replay::build_between(&s, c, Some(&HaltRegs(entry)), None, &mut |_, _| None, 0);
    assert!(!reg(&other, Some(0), "d8").is_known());
}
