import * as assert from 'assert';
import { describeCores, parseBreakpointCondition, parseCoreRef, splitCoreSuffix, withCoreSuffix } from '../../breakpointCondition';

suite('Breakpoint conditions', () => {
	test('splits a core suffix off a function breakpoint name', () => {
		assert.deepStrictEqual(splitCoreSuffix('Os_Task_10ms@1'), { text: 'Os_Task_10ms', cores: [1] });
		assert.deepStrictEqual(splitCoreSuffix('main.c:42 @ 2, 0,2'), { text: 'main.c:42', cores: [0, 2] });
		assert.deepStrictEqual(splitCoreSuffix('0x80000100@0'), { text: '0x80000100', cores: [0] });
		assert.deepStrictEqual(splitCoreSuffix('main'), { text: 'main' });
		assert.deepStrictEqual(splitCoreSuffix('a@b'), { text: 'a@b' });
	});

	test('reads task filters and cores from a condition', () => {
		assert.deepStrictEqual(parseBreakpointCondition('@1'), { cores: [1] });
		assert.deepStrictEqual(parseBreakpointCondition('task == ComTask @1'), { task: { task: 'ComTask', negate: false }, cores: [1] });
		assert.deepStrictEqual(parseBreakpointCondition('task != Idle'), { task: { task: 'Idle', negate: true }, cores: undefined });
		assert.deepStrictEqual(parseBreakpointCondition(undefined), { cores: undefined });
		assert.deepStrictEqual(parseBreakpointCondition('  '), { cores: undefined });
	});

	test('passes any other condition on as an expression, keeping the cores', () => {
		assert.deepStrictEqual(parseBreakpointCondition('x > 3'), { cores: undefined, expression: 'x > 3' });
		assert.deepStrictEqual(parseBreakpointCondition('x > 3 @0'), { cores: [0], expression: 'x > 3' });
		assert.deepStrictEqual(parseBreakpointCondition('g_s.mode == 2 && g_n != 0'), { cores: undefined, expression: 'g_s.mode == 2 && g_n != 0' });
	});

	test('replaces the core suffix and keeps the rest', () => {
		assert.strictEqual(withCoreSuffix('task == ComTask @1', [2, 0]), 'task == ComTask @0,2');
		assert.strictEqual(withCoreSuffix('task == ComTask @1', []), 'task == ComTask');
		assert.strictEqual(withCoreSuffix('', [1]), '@1');
		assert.strictEqual(withCoreSuffix('@1', undefined), '');
	});

	test('names the HSM as @hsm, after the numbered cores', () => {
		assert.deepStrictEqual(splitCoreSuffix('HsmMain@hsm'), { text: 'HsmMain', cores: ['hsm'] });
		assert.deepStrictEqual(splitCoreSuffix('main.c:42 @HSM, 1'), { text: 'main.c:42', cores: [1, 'hsm'] });
		assert.deepStrictEqual(parseBreakpointCondition('task == ComTask @0,hsm'), { task: { task: 'ComTask', negate: false }, cores: [0, 'hsm'] });
		assert.strictEqual(withCoreSuffix('@1', ['hsm', 0]), '@0,hsm');
		assert.deepStrictEqual(splitCoreSuffix('a@hsmx'), { text: 'a@hsmx' });
	});

	test('names the SCR as @scr, after the HSM', () => {
		assert.deepStrictEqual(splitCoreSuffix('scr_main@scr'), { text: 'scr_main', cores: ['scr'] });
		assert.deepStrictEqual(splitCoreSuffix('0x0100 @SCR'), { text: '0x0100', cores: ['scr'] });
		assert.deepStrictEqual(splitCoreSuffix('main.c:42 @scr, hsm, 1'), { text: 'main.c:42', cores: [1, 'hsm', 'scr'] });
		assert.deepStrictEqual(parseBreakpointCondition('x > 3 @scr'), { cores: ['scr'], expression: 'x > 3' });
		assert.strictEqual(withCoreSuffix('task == ComTask @1', ['scr', 0, 'hsm']), 'task == ComTask @0,hsm,scr');
		assert.deepStrictEqual(splitCoreSuffix('a@scrx'), { text: 'a@scrx' });
		assert.deepStrictEqual(splitCoreSuffix('a@scr,scr'), { text: 'a', cores: ['scr'] });
	});

	test('parses a single core reference', () => {
		assert.strictEqual(parseCoreRef('3'), 3);
		assert.strictEqual(parseCoreRef(' HSM '), 'hsm');
		assert.strictEqual(parseCoreRef('Scr'), 'scr');
		assert.strictEqual(parseCoreRef('ppu'), undefined);
		assert.strictEqual(parseCoreRef(''), undefined);
	});

	test('describes cores for the hover', () => {
		assert.strictEqual(describeCores([1]), 'core 1');
		assert.strictEqual(describeCores([0, 2]), 'cores 0, 2');
		assert.strictEqual(describeCores(['hsm']), 'the HSM');
		assert.strictEqual(describeCores(['hsm', 0]), 'core 0 and the HSM');
		assert.strictEqual(describeCores(['scr']), 'the SCR');
		assert.strictEqual(describeCores([2, 'scr']), 'core 2 and the SCR');
		assert.strictEqual(describeCores(['scr', 'hsm', 1, 0]), 'cores 0, 1, the HSM and the SCR');
	});
});
