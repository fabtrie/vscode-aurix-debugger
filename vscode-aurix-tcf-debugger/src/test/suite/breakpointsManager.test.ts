import * as assert from 'assert';
import { BreakpointsManager, sourceKey } from '../../breakpointsManager';

// Before the first stop VS Code sends the editor's path, afterwards the stack frame's.
const EDITOR_PATH = String.raw`c:\workspace\demo\Cpu0_Main.c`;
const FRAME_PATH = String.raw`C:\workspace\demo\Cpu0_Main.c`;

suite('Source breakpoints', () => {
	test('one file spelled two ways is one file on Windows', () => {
		assert.strictEqual(sourceKey(EDITOR_PATH, 'win32'), sourceKey(FRAME_PATH, 'win32'));
		assert.strictEqual(sourceKey(EDITOR_PATH, 'win32'), sourceKey(String.raw`C:\workspace\demo\build\..\Cpu0_Main.c`, 'win32'));
		assert.notStrictEqual(sourceKey('/work/Main.c', 'linux'), sourceKey('/work/main.c', 'linux'));
	});

	test('a removal sent with the other spelling removes the breakpoint', () => {
		const m = new BreakpointsManager(f => sourceKey(f, 'win32'));
		const [bp] = m.updateFile(EDITOR_PATH, [41]);
		const after = m.updateFile(FRAME_PATH, [17]);
		assert.deepStrictEqual(after.map(b => b.line), [17]);
		assert.deepStrictEqual(m.getAll().map(b => b.line), [17]);
		assert.strictEqual(m.getById(bp.id), undefined);
	});

	test('a breakpoint that stays keeps its ID and takes the latest spelling', () => {
		const m = new BreakpointsManager(f => sourceKey(f, 'win32'));
		const [bp] = m.updateFile(EDITOR_PATH, [41]);
		const [again] = m.updateFile(FRAME_PATH, [41]);
		assert.strictEqual(again.id, bp.id);
		assert.strictEqual(again.file, FRAME_PATH);
		assert.strictEqual(m.getAll().length, 1);
		assert.ok(m.deleteById(bp.id));
		assert.deepStrictEqual(m.getAll(), []);
		// The file's map is empty too: a new breakpoint on the line gets a new ID.
		const [fresh] = m.updateFile(EDITOR_PATH, [41]);
		assert.notStrictEqual(fresh.id, bp.id);
	});
});
