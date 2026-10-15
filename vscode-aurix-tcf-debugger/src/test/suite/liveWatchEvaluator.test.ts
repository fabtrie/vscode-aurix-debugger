import * as assert from 'assert';
import { LiveWatchEvaluator } from '../../liveWatchEvaluator';
import { CreateExpressionsCommand, DisposeExpressionsCommand } from '../../tcf/expressions';
import type { TCFClient } from '../../tcfclient';

/**
 * Counts the expressions created in the agent. The value read is a plain 4-byte number;
 * only the creation is under test. (A read that fails makes the evaluator create the root again.)
 */
function fakeClient(): { client: TCFClient; created: () => number; disposed: () => number } {
	let created = 0;
	let disposed = 0;
	const client = {
		sendCommand: async (command: unknown) => {
			if (command instanceof CreateExpressionsCommand) {
				created++;
				// Long enough for a second request to arrive while the first one is still creating.
				await new Promise((resolve) => setTimeout(resolve, 20));
				return `expr${created}`;
			}
			if (command instanceof DisposeExpressionsCommand) {
				disposed++;
				return undefined;
			}
			if (command?.constructor?.name === 'EvaluateExpressionsCommand') {
				return { value: Buffer.from([1, 0, 0, 0]), properties: { Type: 'type1' } };
			}
			if (command?.constructor?.name === 'GetContextSymbolsCommand') {
				return { ID: 'type1', TypeClass: 1, Size: 4 };
			}
			throw new Error(`unexpected command ${command?.constructor?.name}`);
		},
	} as unknown as TCFClient;
	return { client, created: () => created, disposed: () => disposed };
}

suite('LiveWatchEvaluator', () => {
	test('two Live Watches asking for the same new expression at once create it once', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const items = [{ expression: 'counter' }];
		await Promise.all([evaluator.evaluate('ctx', items), evaluator.evaluate('ctx', items)]);
		assert.strictEqual(fake.created(), 1);
		// Later refreshes reuse it.
		await evaluator.evaluate('ctx', items);
		assert.strictEqual(fake.created(), 1);
		// And clearing disposes the one there is.
		evaluator.clear();
		assert.strictEqual(fake.disposed(), 1);
	});
});
