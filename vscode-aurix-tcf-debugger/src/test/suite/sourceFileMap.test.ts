import * as assert from 'assert';
import * as path from 'path';
import { deriveMapping, SourceFileResolver, SourcePrefixMapping } from '../../sourceFileMap';

const local = (p: string) => path.resolve(p);

/** A file system holding exactly `files`. */
function fakeFs(files: string[]): (p: string) => boolean {
	const set = new Set(files.map(f => local(f).toLowerCase()));
	return (p: string) => set.has(local(p).toLowerCase());
}

const DWARF = 'c:\\git\\project\\app\\bsw\\Os\\src\\Os_SystemTasks.c';
const CHECKOUT = 'c:\\workspace\\project';
const LOCAL = 'c:\\workspace\\project\\app\\bsw\\Os\\src\\Os_SystemTasks.c';

suite('Source file map', () => {
	test('explicit mapping wins, also over an existing original path', () => {
		const r = new SourceFileResolver({ 'C:/git/project/': CHECKOUT }, [], undefined, fakeFs([DWARF, LOCAL]));
		assert.strictEqual(r.toLocal(DWARF), local(LOCAL));
	});

	test('existing paths are kept without a mapping', () => {
		const r = new SourceFileResolver(undefined, [CHECKOUT], undefined, fakeFs([DWARF, LOCAL]));
		assert.strictEqual(r.toLocal(DWARF), DWARF);
	});

	test('suffix search finds the file and learns the checkout mapping', () => {
		const saved: SourcePrefixMapping[][] = [];
		const other = 'c:\\git\\project\\app\\bsw\\Os\\src\\Os_Hooks.c';
		const r = new SourceFileResolver(undefined, ['c:\\workspace'], { load: () => [], save: m => saved.push(m) },
			fakeFs([LOCAL, 'c:\\workspace\\project\\app\\bsw\\Os\\src\\Os_Hooks.c']));
		assert.strictEqual(r.toLocal(DWARF), local(LOCAL));
		// "project" is common to both checkouts as well, so the mapping is one folder up.
		assert.deepStrictEqual(r.mappings, [{ from: 'c:/git', to: local('c:\\workspace') }]);
		assert.strictEqual(saved.length, 1);
		assert.strictEqual(r.toLocal(other), local('c:\\workspace\\project\\app\\bsw\\Os\\src\\Os_Hooks.c'));
	});

	test('a bare file name never matches', () => {
		const r = new SourceFileResolver(undefined, ['c:\\workspace'], undefined, fakeFs(['c:\\workspace\\Os_SystemTasks.c']));
		assert.strictEqual(r.toLocal(DWARF), undefined);
	});

	test('learned mappings are loaded from the store', () => {
		const r = new SourceFileResolver(undefined, [], { load: () => [{ from: 'c:/git/project', to: CHECKOUT }], save: () => undefined },
			fakeFs([LOCAL]));
		assert.strictEqual(r.toLocal(DWARF), local(LOCAL));
	});

	test('a later mapping resolves paths that failed before', () => {
		const r = new SourceFileResolver(undefined, [], undefined, fakeFs([LOCAL]));
		assert.strictEqual(r.toLocal(DWARF), undefined);
		r.learn(deriveMapping(DWARF, LOCAL)!);
		assert.strictEqual(r.toLocal(DWARF), local(LOCAL));
	});

	test('breakpoint paths below a mapping are sent relative to the mapping root', () => {
		const r = new SourceFileResolver({ 'c:/git/project': CHECKOUT }, [], undefined, fakeFs([]));
		assert.strictEqual(r.toDebugger(LOCAL), 'app/bsw/Os/src/Os_SystemTasks.c');
		assert.strictEqual(r.toDebugger('c:\\other\\x.c'), undefined);
	});

	test('deriveMapping strips the common trailing folders', () => {
		assert.deepStrictEqual(deriveMapping(DWARF, LOCAL), { from: 'c:/git', to: local('c:\\workspace') });
		assert.strictEqual(deriveMapping(DWARF, 'c:\\workspace\\other.c'), undefined);
	});
});
