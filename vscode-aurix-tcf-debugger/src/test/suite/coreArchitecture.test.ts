import * as assert from 'assert';
import {
	alignInstructionAddress,
	clampDisassemblyWindow,
	contextMachine,
	inCodeSpace,
	isaGeometry,
	isaOfMachine,
	isIndependentCore,
	MACHINE_ARM,
	MACHINE_MCS_ASM,
	MACHINE_TRICORE,
	MACHINE_XC800,
	MCS_RAM_BYTES,
} from '../../coreArchitecture';

suite('Core architectures', () => {
	test('maps the RunControl Architecture property to a machine', () => {
		assert.strictEqual(contextMachine({ Architecture: 'xc800' }), MACHINE_XC800);
		assert.strictEqual(contextMachine({ Architecture: 'XC800' }), MACHINE_XC800);
		assert.strictEqual(MACHINE_XC800, 165);
		assert.strictEqual(contextMachine({ Architecture: 'arm' }), MACHINE_ARM);
		assert.strictEqual(contextMachine({ Architecture: 'gtm_mcs' }), MACHINE_MCS_ASM);
		assert.strictEqual(contextMachine({ Architecture: 'tricore' }), MACHINE_TRICORE);
		assert.strictEqual(contextMachine({}), MACHINE_TRICORE);
		assert.strictEqual(contextMachine(undefined), MACHINE_TRICORE);
	});

	test('the MCS channels and the SCR are independent of TL1, the TriCores and the HSM are not', () => {
		assert.strictEqual(isIndependentCore(MACHINE_XC800), true);
		assert.strictEqual(isIndependentCore(MACHINE_MCS_ASM), true);
		assert.strictEqual(isIndependentCore(MACHINE_TRICORE), false);
		assert.strictEqual(isIndependentCore(MACHINE_ARM), false);
		assert.strictEqual(isIndependentCore(undefined), false);
	});

	test('knows the disassembly ISA of each machine', () => {
		assert.strictEqual(isaOfMachine(MACHINE_XC800), 'XC800');
		assert.strictEqual(isaOfMachine(MACHINE_MCS_ASM), 'MCS');
		assert.strictEqual(isaOfMachine(MACHINE_TRICORE), 'TRICORE');
		assert.strictEqual(isaOfMachine(undefined), 'TRICORE');
	});

	test('XC800 instructions are 1 to 3 bytes at any address in a 64 KB code space', () => {
		const g = isaGeometry('XC800');
		assert.deepStrictEqual(g, { align: 1, maxBytes: 3, codeEnd: 0x10000 });
		assert.deepStrictEqual(isaGeometry('xc800'), g);
		assert.strictEqual(alignInstructionAddress(0x1235, g), 0x1235);
		assert.strictEqual(alignInstructionAddress(0x0001, g), 0x0001);
	});

	test('TriCore and MCS addresses are aligned as before', () => {
		const tricore = isaGeometry('TRICORE');
		assert.deepStrictEqual(tricore, { align: 2, maxBytes: 4 });
		assert.strictEqual(alignInstructionAddress(0x80001235, tricore), 0x80001234);
		assert.deepStrictEqual(isaGeometry(undefined), tricore);
		const mcs = isaGeometry('MCS');
		assert.deepStrictEqual(mcs, { align: 4, maxBytes: 4, codeEnd: MCS_RAM_BYTES });
		assert.strictEqual(alignInstructionAddress(0x106, mcs), 0x104);
	});

	test('clamps an XC800 disassembly window to 0..0xFFFF without aligning it', () => {
		const g = isaGeometry('XC800');
		// Before the start of the code space: starts at 0, keeps its size.
		assert.deepStrictEqual(clampDisassemblyWindow(-0x60, 0x90, g), { start: 0, bytes: 0x90 });
		// Odd addresses stay as they are.
		assert.deepStrictEqual(clampDisassemblyWindow(0x1233, 0x40, g), { start: 0x1233, bytes: 0x40 });
		// Past the top: cut at 0x10000.
		assert.deepStrictEqual(clampDisassemblyWindow(0xffe0, 0x90, g), { start: 0xffe0, bytes: 0x20 });
		// Entirely past the top: nothing.
		assert.strictEqual(clampDisassemblyWindow(0x10000, 0x10, g), undefined);
		assert.strictEqual(clampDisassemblyWindow(0x12345, 0x10, g), undefined);
	});

	test('clamps an MCS window to the module RAM, aligned to 4', () => {
		const g = isaGeometry('MCS');
		assert.deepStrictEqual(clampDisassemblyWindow(-8, 0x40, g), { start: 0, bytes: 0x40 });
		assert.deepStrictEqual(clampDisassemblyWindow(0x102, 0x40, g), { start: 0x100, bytes: 0x40 });
		assert.deepStrictEqual(clampDisassemblyWindow(MCS_RAM_BYTES - 8, 0x40, g), { start: MCS_RAM_BYTES - 8, bytes: 8 });
	});

	test('leaves a TriCore window alone', () => {
		const g = isaGeometry('TRICORE');
		assert.deepStrictEqual(clampDisassemblyWindow(0x80000000, 0x100, g), { start: 0x80000000, bytes: 0x100 });
	});

	test('checks Go to Address targets against the code space', () => {
		const g = isaGeometry('XC800');
		assert.strictEqual(inCodeSpace(0, g), true);
		assert.strictEqual(inCodeSpace(0xffff, g), true);
		assert.strictEqual(inCodeSpace(0x10000, g), false);
		assert.strictEqual(inCodeSpace(0x80000000, g), false);
		assert.strictEqual(inCodeSpace(-1, g), false);
		assert.strictEqual(inCodeSpace(0x80000000, isaGeometry('TRICORE')), true);
	});
});
