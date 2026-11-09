/**
 * The architectures of the cores a session debugs, and what the debug adapter has to know of
 * their instructions: the TriCores (and the Arm HSM) share the TriCore address space, while the
 * GTM MCS channels and the TC3xx standby controller (SCR, an XC800/8051 core) each have their own
 * code space starting at 0.
 *
 * Kept free of the vscode API, so the debug adapter and its tests can use it.
 */

/** ELF e_machine of the Arm HSM. */
export const MACHINE_ARM = 40;
/** ELF e_machine of the TriCore. */
export const MACHINE_TRICORE = 44;
/** Symbol file kind of GTM MCS assembler sources (no ELF: the server assembles them). */
export const MACHINE_MCS_ASM = -1;
/** ELF e_machine of the 8051 (EM_8051), the XC800 core of the TC3xx standby controller (SCR). */
export const MACHINE_XC800 = 165;

/** Size of a GTM MCS module RAM, the whole MCS program space (3072 words, TC3xx UM MCS chapter). */
export const MCS_RAM_BYTES = 3072 * 4;
/** Size of the XC800 code space (16-bit program counter). */
export const XC800_CODE_BYTES = 0x10000;

/** ELF e_machine of a RunControl context's architecture ("Architecture" property; default TriCore). */
export function contextMachine(info: { Architecture?: unknown } | undefined): number {
	const arch = typeof info?.Architecture === 'string' ? info.Architecture.toLowerCase() : 'tricore';
	switch (arch) {
		case 'arm': return MACHINE_ARM;
		case 'gtm_mcs': return MACHINE_MCS_ASM;
		case 'xc800': return MACHINE_XC800;
		default: return MACHINE_TRICORE;
	}
}

/**
 * The machine a symbol file is for, when its name tells: a GTM MCS assembler source (`.asm`) or an
 * SDCC debug file (`.cdb`, the SCR's program). `undefined` for anything else (an ELF: read its
 * header).
 */
export function symbolFileMachine(file: string): number | undefined {
	if (/\.asm$/i.test(file)) {
		return MACHINE_MCS_ASM;
	}
	if (/\.cdb$/i.test(file)) {
		return MACHINE_XC800;
	}
	return undefined;
}

/**
 * Whether a core runs on its own, outside the OTGS trigger line TL1 that halts the TriCores and
 * the HSM together: a GTM MCS channel or the SCR. Its stop stops nothing else, and pausing the
 * system leaves it running.
 */
export function isIndependentCore(machine: number | undefined): boolean {
	return machine === MACHINE_MCS_ASM || machine === MACHINE_XC800;
}

/** How a core's instructions lie in its code space. */
export interface IsaGeometry {
	/** Instructions start on addresses that are a multiple of this. */
	align: number;
	/** Bytes of the longest instruction. */
	maxBytes: number;
	/** A code space of its own, `[0, end)`, apart from the ELF's (the TriCore address space). */
	codeEnd?: number;
}

/** The instruction geometry of a disassembly ISA (`TRICORE`, `MCS`, `XC800`, ...). */
export function isaGeometry(isa: string | undefined): IsaGeometry {
	switch ((isa ?? '').toUpperCase()) {
		case 'MCS': return { align: 4, maxBytes: 4, codeEnd: MCS_RAM_BYTES };
		// 8051 instructions are 1 to 3 bytes, at any address.
		case 'XC800': return { align: 1, maxBytes: 3, codeEnd: XC800_CODE_BYTES };
		// TriCore instructions are 2 or 4 bytes, as are the HSM's Thumb instructions.
		default: return { align: 2, maxBytes: 4 };
	}
}

/** The disassembly ISA of a core architecture (see `contextMachine`). */
export function isaOfMachine(machine: number | undefined): string {
	switch (machine) {
		case MACHINE_MCS_ASM: return 'MCS';
		case MACHINE_XC800: return 'XC800';
		case MACHINE_ARM: return 'ARM';
		default: return 'TRICORE';
	}
}

/** `addr` moved down to the start of an instruction slot of `geometry`. */
export function alignInstructionAddress(addr: number, geometry: IsaGeometry): number {
	return addr - (((addr % geometry.align) + geometry.align) % geometry.align);
}

/**
 * The disassembly window `[start, start + bytes)` kept inside the code space of `geometry` (when
 * it has one of its own) with an aligned start, or undefined when nothing of it is left.
 *
 * VS Code also asks for the instructions before the PC, which reaches below 0 near the start of
 * the code space (and past its end near the top); the server refuses those.
 */
export function clampDisassemblyWindow(start: number, bytes: number, geometry: IsaGeometry): { start: number; bytes: number } | undefined {
	if (geometry.codeEnd === undefined) {
		return { start, bytes };
	}
	const s = alignInstructionAddress(Math.max(0, start), geometry);
	const n = Math.min(bytes, geometry.codeEnd - s);
	return n > 0 ? { start: s, bytes: n } : undefined;
}

/** Whether `addr` is in the code space of `geometry`; any address is when it has none of its own. */
export function inCodeSpace(addr: number, geometry: IsaGeometry): boolean {
	return geometry.codeEnd === undefined || (addr >= 0 && addr < geometry.codeEnd);
}
