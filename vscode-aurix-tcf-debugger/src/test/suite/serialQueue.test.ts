import * as assert from 'assert';
import { SerialQueue } from '../../serialQueue';

const tick = (ms: number) => new Promise<void>(r => setTimeout(r, ms));

suite('Serial queue', () => {
	test('runs jobs one after the other, in the order they were queued', async () => {
		const q = new SerialQueue();
		const log: string[] = [];
		// The first job takes longest: without the queue its result would come last.
		const a = q.run(async () => { log.push('a start'); await tick(30); log.push('a end'); return 'a'; });
		const b = q.run(async () => { log.push('b start'); await tick(5); log.push('b end'); return 'b'; });
		assert.deepStrictEqual(await Promise.all([a, b]), ['a', 'b']);
		assert.deepStrictEqual(log, ['a start', 'a end', 'b start', 'b end']);
	});

	test('a failed job does not stop the ones after it', async () => {
		const q = new SerialQueue();
		const failed = q.run(async () => { throw new Error('boom'); });
		const next = q.run(async () => 'ran');
		await assert.rejects(failed, /boom/);
		assert.strictEqual(await next, 'ran');
	});

	test('a job reads the state when it is its turn, not when it was queued', async () => {
		// As syncAllBreakpoints: the list is built when the sync runs, so the last one always
		// sends the latest breakpoints.
		const q = new SerialQueue();
		let breakpoints = ['line 41'];
		const sent: string[][] = [];
		const sync = () => q.run(async () => { const list = [...breakpoints]; await tick(10); sent.push(list); });
		const first = sync();
		await tick(2); // the first sync has built its list and waits for the answer
		breakpoints = [];
		const second = sync();
		await Promise.all([first, second]);
		assert.deepStrictEqual(sent, [['line 41'], []]);
	});
});
