import * as assert from 'assert';
import { LiveWatchEvaluator, liveWatchChildExpression, splitLiveWatchCore } from '../../liveWatchEvaluator';
import { CreateExpressionsCommand, DisposeExpressionsCommand } from '../../tcf/expressions';
import type { TCFClient } from '../../tcfclient';
import { DefaultVariableHelper } from '../../variables/helper';
import { ArrayVariable } from '../../variables/array';
import { FixedNameProvider } from '../../variables/types';
import { FindByAddrSymbolsCommand, GetChildrenSymbolsCommand } from '../../tcf/symbols';
import { PointerVariable } from '../../variables/pointer';
import type { SimpleCommand } from '../../tcf/tcfutils';

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

	test('expr@N names the core, and children keep it', () => {
		assert.deepStrictEqual(splitLiveWatchCore('COSI_DOOR_data@1'), { expression: 'COSI_DOOR_data', core: 1 });
		assert.deepStrictEqual(splitLiveWatchCore(' a.b[2] @ 3 '), { expression: 'a.b[2]', core: 3 });
		assert.deepStrictEqual(splitLiveWatchCore('counter'), { expression: 'counter' });
		assert.strictEqual(liveWatchChildExpression('COSI_DOOR_data@1', '[0]'), 'COSI_DOOR_data[0]@1');
		assert.strictEqual(liveWatchChildExpression('p@2', '*'), '*p@2');
		assert.strictEqual(liveWatchChildExpression('a + b@0', 'x'), '(a + b).x@0');
		assert.strictEqual(liveWatchChildExpression('s', 'x'), 's.x');
	});

	test('expr@N is evaluated in the context of core N', async () => {
		const fake = fakeClient();
		const asked: number[] = [];
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined, async (core) => {
			asked.push(core);
			return core === 1 ? 'S0.D0.C1' : undefined;
		});
		const [ok, missing] = await evaluator.evaluate('S0.D0.C0', [{ expression: 'counter@1' }, { expression: 'counter@7' }]);
		assert.deepStrictEqual(asked.sort(), [1, 7]);
		assert.strictEqual(ok.available, true);
		assert.strictEqual(ok.name, 'counter@1');
		assert.strictEqual(missing.available, false);
		assert.match(missing.error ?? '', /No core 7/);
	});
});

suite('ArrayVariable', () => {
	test('an array of structs has one struct child per element, with its fields', async () => {
		const symbols: Record<string, unknown> = {
			elem: { ID: 'elem', Class: 4, TypeClass: 6, Size: 4, Name: 'elem_t' },
			a: { ID: 'a', Class: 2, Name: 'a', Offset: 0, Size: 2, TypeID: 'u16' },
			b: { ID: 'b', Class: 2, Name: 'b', Offset: 2, Size: 2, TypeID: 'u16' },
			u16: { ID: 'u16', Class: 4, TypeClass: 2, Size: 2, Name: 'uint16' },
		};
		const helper = new class extends DefaultVariableHelper {
			log(): void { }
			async sendCommand<T>(c: SimpleCommand<T>): Promise<T> {
				const id = (c as any).contextID;
				if (c instanceof GetChildrenSymbolsCommand) {
					return (id === 'elem' ? ['a', 'b'] : []) as T;
				}
				return symbols[id] as T;
			}
		}();
		const type = { ID: 'arr', Class: 4, TypeClass: 5, Size: 8, Length: 2, BaseTypeID: 'elem', Name: 'elem_t[2]' } as any;
		const value = { value: async () => Buffer.from([1, 0, 2, 0, 3, 0, 4, 0]), isBigEndian: () => false };
		const arr = new ArrayVariable(type, helper, new FixedNameProvider('x'), value);
		const elems = await arr.getChildren();
		assert.strictEqual(elems.length, 2);
		const fields = await (elems[1] as any).getChildren();
		assert.deepStrictEqual(fields.map((f: any) => f.name()), ['a', 'b']);
	});
});

suite('PointerVariable', () => {
	test('a function pointer shows the function it points to', async () => {
		const symbols: Record<string, unknown> = {
			fnType: { ID: 'fnType', Class: 4, TypeClass: 8, Size: 0 },
			fn: { ID: 'fn', Class: 3, TypeClass: 8, Name: 'COSI_DOOR_state_init', Address: 0x803017a4, Size: 110 },
		};
		const helper = new class extends DefaultVariableHelper {
			log(): void { }
			async sendCommand<T>(c: SimpleCommand<T>): Promise<T> {
				if (c instanceof FindByAddrSymbolsCommand) {
					if (c.address < 0x803017a4 || c.address >= 0x803017a4 + 110) {
						throw new Error('No symbol');
					}
					return 'fn' as T;
				}
				return symbols[(c as any).contextID] as T;
			}
		}();
		const type = { ID: 'ptr', Class: 4, TypeClass: 4, Size: 4, BaseTypeID: 'fnType' } as any;
		const show = (addr: number) => {
			const raw = Buffer.alloc(4);
			raw.writeUInt32LE(addr);
			return new PointerVariable(type, undefined, new FixedNameProvider('p'), { value: async () => raw, isBigEndian: () => false }, helper).displayValue();
		};
		assert.strictEqual(await show(0x803017a4), '0x803017a4 <COSI_DOOR_state_init>');
		assert.strictEqual(await show(0x803017b0), '0x803017b0 <COSI_DOOR_state_init+0xc>');
		assert.strictEqual(await show(0x90000000), '0x90000000');
		assert.strictEqual(await show(0), '0x0');
	});
});
