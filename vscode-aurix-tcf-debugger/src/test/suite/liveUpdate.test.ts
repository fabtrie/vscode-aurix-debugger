import * as assert from 'assert';
import { LiveUpdater, notifyLiveUpdateBreak, setLiveUpdateIntervalMs, setLiveUpdateSessionActive } from '../../liveUpdate';

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** An updater that is on screen and counts its refreshes. */
function counting(): { updater: LiveUpdater; periodic: () => number; onBreak: () => number } {
	let periodic = 0;
	let onBreak = 0;
	const updater = new LiveUpdater({
		refresh: async (isBreak) => {
			if (isBreak) {
				onBreak++;
			} else {
				periodic++;
			}
		},
	});
	updater.setVisible(true);
	return { updater, periodic: () => periodic, onBreak: () => onBreak };
}

suite('LiveUpdater interval per view', () => {
	const updaters: LiveUpdater[] = [];

	setup(() => {
		setLiveUpdateIntervalMs(1000);
		setLiveUpdateSessionActive(false);
	});

	teardown(() => {
		updaters.splice(0).forEach((u) => u.dispose());
		setLiveUpdateSessionActive(false);
		setLiveUpdateIntervalMs(1000);
	});

	test('the global interval leaves a view with its own interval alone', () => {
		const own = new LiveUpdater({ refresh: async () => undefined });
		const shared = new LiveUpdater({ refresh: async () => undefined });
		updaters.push(own, shared);
		own.setIntervalOverride(500);
		setLiveUpdateIntervalMs(2000);
		assert.strictEqual(own.intervalMs, 500);
		assert.strictEqual(own.intervalOverrideMs, 500);
		assert.strictEqual(shared.intervalMs, 2000);
		assert.strictEqual(shared.intervalOverrideMs, undefined);
	});

	test('clearing the own interval follows the global one again, including later changes', () => {
		const u = new LiveUpdater({ refresh: async () => undefined });
		updaters.push(u);
		u.setIntervalOverride(300);
		setLiveUpdateIntervalMs(1500);
		u.setIntervalOverride(undefined);
		assert.strictEqual(u.intervalMs, 1500);
		setLiveUpdateIntervalMs(700);
		assert.strictEqual(u.intervalMs, 700);
	});

	test('an own interval is limited like the global one', () => {
		const u = new LiveUpdater({ refresh: async () => undefined });
		updaters.push(u);
		u.setIntervalOverride(10);
		assert.strictEqual(u.intervalMs, 200);
		u.setIntervalOverride(60_000);
		assert.strictEqual(u.intervalMs, 10_000);
	});

	test('views refresh at their own rates', async () => {
		const fast = counting();
		const slow = counting();
		updaters.push(fast.updater, slow.updater);
		fast.updater.setIntervalOverride(200);
		setLiveUpdateIntervalMs(5000);
		setLiveUpdateSessionActive(true);
		await wait(1100);
		// The first refresh is immediate, then one every 200 ms: about 6 here.
		assert.ok(fast.periodic() >= 4, `fast view refreshed ${fast.periodic()} times`);
		assert.strictEqual(slow.periodic(), 1);
	});

	test('a break refreshes every visible view, whatever its interval', async () => {
		const a = counting();
		const b = counting();
		const hidden = counting();
		updaters.push(a.updater, b.updater, hidden.updater);
		a.updater.setIntervalOverride(200);
		hidden.updater.setVisible(false);
		setLiveUpdateSessionActive(true);
		notifyLiveUpdateBreak();
		await wait(250);
		assert.strictEqual(a.onBreak(), 1);
		assert.strictEqual(b.onBreak(), 1);
		assert.strictEqual(hidden.onBreak(), 0);
	});
});
