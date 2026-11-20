import * as assert from 'assert';
import { StoppedDetailsMirror } from '../../stoppedDetailsMirror';

// Thread IDs as the adapter assigns them to the TL1 cores.
const CPU0 = 1;
const CPU1 = 2;
const CPU2 = 3;

suite('Stopped details mirror', () => {
	test('a breakpoint stop with allThreadsStopped covers the other cores until cpu0 continues', () => {
		const m = new StoppedDetailsMirror();
		m.stopped(CPU0, true);
		assert.deepStrictEqual(m.uncovered([CPU0, CPU1, CPU2]), []);

		// Step Back (or a step) on cpu0: the server resumes and halts only cpu0. VS Code forgets the
		// stop that also stood for cpu1 and cpu2.
		m.continued(CPU0);
		m.stopped(CPU0, false);
		assert.deepStrictEqual(m.uncovered([CPU0, CPU1, CPU2]), [CPU1, CPU2]);

		// Their own stops keep them covered through the next steps.
		m.stopped(CPU1, false);
		m.stopped(CPU2, false);
		m.continued(CPU0);
		m.stopped(CPU0, false);
		assert.deepStrictEqual(m.uncovered([CPU0, CPU1, CPU2]), []);
	});

	test('a core that continued is not covered by its own old stop', () => {
		const m = new StoppedDetailsMirror();
		m.stopped(CPU1, false);
		m.continued(CPU1);
		assert.strictEqual(m.covers(CPU1), false);
		m.stopped(CPU0, true);
		assert.strictEqual(m.covers(CPU1), true, 'allThreadsStopped covers every thread');
		m.clear();
		assert.deepStrictEqual(m.uncovered([CPU0]), [CPU0]);
	});
});
