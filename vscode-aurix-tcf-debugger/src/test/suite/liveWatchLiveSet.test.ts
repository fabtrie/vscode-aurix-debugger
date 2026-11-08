// The fake agent answers with TCF-shaped JSON (PascalCase names) and throws TCF errors as plain objects, as the real client rejects with them.
/* eslint-disable no-throw-literal, @typescript-eslint/naming-convention */
import * as assert from 'assert';
import * as vscode from 'vscode';
import { LiveWatchEvaluator, LiveWatchRequestItem } from '../../liveWatchEvaluator';
import { LiveWatchInstance } from '../../liveWatchView';
import { setLiveUpdateIntervalMs, setLiveUpdateSessionActive } from '../../liveUpdate';
import { ClearLiveSetAurixTargetCommand, GetLiveValuesAurixTargetCommand, LiveRegion, LiveValue, SetLiveSetAurixTargetCommand } from '../../tcf/aurixtarget';
import { CreateExpressionsCommand, DisposeExpressionsCommand, EvaluateExpressionsCommand } from '../../tcf/expressions';
import { GetMemoryCommand } from '../../tcf/memory';
import { GetContextSymbolsCommand } from '../../tcf/symbols';
import { ServiceNotSupportedError } from '../../tcf/tcfutils';
import type { TCFClient } from '../../tcfclient';

/** Variables of the fake program: a 4-byte unsigned at an address, owned by a core or not. */
const VARIABLES: Record<string, { address: number; owner?: string }> = {
	a: { address: 0x70000000 },
	b: { address: 0x70000004 },
	local1: { address: 0xd0000010, owner: 'S0.D0.C1' },
};

/** The value the fake target has at an address: 4 bytes, the low byte of the address first. */
function memory(addr: number, size: number): Buffer {
	const b = Buffer.alloc(size);
	b[0] = addr & 0xff;
	return b;
}

type FakeOptions = {
	/** Replaces the server's answer to setLiveSet (throw to fail it). */
	setLiveSet?: (c: SetLiveSetAurixTargetCommand) => void;
	/** Replaces the server's answer to getLiveValues for the current set. */
	getLiveValues?: (regions: LiveRegion[]) => LiveValue[];
};

/** A TCF client with the fake program, a live-set server, and a log of the commands sent. */
function fakeClient(options: FakeOptions = {}) {
	const sent: unknown[] = [];
	const sets = new Map<string, LiveRegion[]>();
	const client = {
		sendCommand: async (c: unknown) => {
			sent.push(c);
			if (c instanceof CreateExpressionsCommand) {
				return `expr:${c.expression}`;
			}
			if (c instanceof DisposeExpressionsCommand) {
				return undefined;
			}
			if (c instanceof EvaluateExpressionsCommand) {
				const name = (c as any).expressionID.replace(/^expr:/, '');
				const v = VARIABLES[name];
				if (!v) {
					throw { Code: 18, Format: `Undefined identifier ${name}` };
				}
				return { value: memory(v.address, 4), properties: { Type: 'u32', Symbol: `sym:${name}` } };
			}
			if (c instanceof GetContextSymbolsCommand) {
				if (c.contextID === 'u32') {
					return { ID: 'u32', Class: 4, TypeClass: 1, Size: 4, Name: 'unsigned int' };
				}
				const name = c.contextID.replace(/^sym:/, '');
				const v = VARIABLES[name];
				return { ID: c.contextID, Class: 2, Name: name, TypeID: 'u32', Size: 4, Address: v.address, OwnerID: v.owner };
			}
			if (c instanceof GetMemoryCommand) {
				return { base64: memory(c.address, c.byteCount).toString('base64') };
			}
			if (c instanceof SetLiveSetAurixTargetCommand) {
				options.setLiveSet?.(c);
				sets.set(c.key, c.regions);
				return undefined;
			}
			if (c instanceof GetLiveValuesAurixTargetCommand) {
				const regions = sets.get(c.key);
				if (!regions) {
					throw { Code: 1, Format: `No live set ${c.key}` };
				}
				return options.getLiveValues
					? options.getLiveValues(regions)
					: regions.map((r) => ({ data: memory(r.addr, r.size).toString('base64'), source: 'read' }));
			}
			if (c instanceof ClearLiveSetAurixTargetCommand) {
				sets.delete(c.key);
				return undefined;
			}
			throw new Error(`unexpected command ${(c as any)?.constructor?.name}`);
		},
	} as unknown as TCFClient;
	const count = (type: Function) => sent.filter((c) => c instanceof type).length;
	return {
		client,
		sent,
		sets,
		setLiveSets: () => sent.filter((c): c is SetLiveSetAurixTargetCommand => c instanceof SetLiveSetAurixTargetCommand),
		count,
		reset: () => sent.splice(0),
	};
}

const LIVE = { viewId: 'sidebar', intervalMs: 500 };

suite('LiveWatchEvaluator live set', () => {
	test('a periodic refresh reads every root with one getLiveValues', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const items = [{ expression: 'a' }, { expression: 'b' }, { expression: 'local1' }];
		const first = await evaluator.evaluate('S0.D0.C0', items, LIVE);
		assert.deepStrictEqual(first.map((r) => [r.available, r.value]), [[true, '0'], [true, '4'], [true, '16']]);
		assert.strictEqual(fake.count(GetLiveValuesAurixTargetCommand), 1);
		assert.strictEqual(fake.count(GetMemoryCommand), 0);
		// The regions, in order; a variable of one core is read with that core.
		assert.deepStrictEqual(fake.setLiveSets().map((c) => [c.key, c.regions, c.periodUs]), [[
			'sidebar',
			[{ ctx: 'S0.D0.C0', addr: 0x70000000, size: 4 }, { ctx: 'S0.D0.C0', addr: 0x70000004, size: 4 }, { ctx: 'S0.D0.C1', addr: 0xd0000010, size: 4 }],
			500_000,
		]]);

		fake.reset();
		await evaluator.evaluate('S0.D0.C0', items, LIVE);
		// Steady state: one request, nothing else.
		assert.strictEqual(fake.sent.length, 1);
		assert.ok(fake.sent[0] instanceof GetLiveValuesAurixTargetCommand);
	});

	test('setLiveSet is sent again only when the regions or the interval change', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		assert.strictEqual(fake.setLiveSets().length, 1);
		// The interval picker (global or the instance's own).
		await evaluator.evaluate('ctx', [{ expression: 'a' }], { ...LIVE, intervalMs: 1000 });
		assert.deepStrictEqual(fake.setLiveSets().map((c) => c.periodUs), [500_000, 1_000_000]);
		// An expression added.
		await evaluator.evaluate('ctx', [{ expression: 'a' }, { expression: 'b' }], { ...LIVE, intervalMs: 1000 });
		await evaluator.evaluate('ctx', [{ expression: 'a' }, { expression: 'b' }], { ...LIVE, intervalMs: 1000 });
		assert.strictEqual(fake.setLiveSets().length, 3);
		assert.strictEqual(fake.setLiveSets()[2].regions.length, 2);
		// Every refresh read once.
		assert.strictEqual(fake.count(GetLiveValuesAurixTargetCommand), 5);
		// Another view has its own set.
		await evaluator.evaluate('ctx', [{ expression: 'a' }], { viewId: 'panel1', intervalMs: 1000 });
		assert.deepStrictEqual(fake.setLiveSets().map((c) => c.key), ['sidebar', 'sidebar', 'sidebar', 'panel1']);
	});

	test('stopping clears the set once, and the next live refresh sets it again', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		await evaluator.stopLive('sidebar');
		assert.strictEqual(fake.sent.length, 0, 'nothing to clear before a set was sent');
		await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		await evaluator.stopLive('sidebar');
		await evaluator.stopLive('sidebar');
		assert.strictEqual(fake.count(ClearLiveSetAurixTargetCommand), 1);
		assert.strictEqual(fake.sets.size, 0);
		await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		assert.strictEqual(fake.setLiveSets().length, 2);
	});

	test('a refresh without live options reads each root with Memory.get', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const values = await evaluator.evaluate('ctx', [{ expression: 'a' }, { expression: 'b' }]);
		assert.deepStrictEqual(values.map((r) => r.value), ['0', '4']);
		assert.strictEqual(fake.count(GetMemoryCommand), 2);
		assert.strictEqual(fake.count(SetLiveSetAurixTargetCommand) + fake.count(GetLiveValuesAurixTargetCommand), 0);
	});

	for (const [what, error] of [
		['an older server (unknown command)', { Code: 25, Format: 'Unsupported command: AurixTarget.setLiveSet' }],
		['a server without the AurixTarget service', new ServiceNotSupportedError('Service AurixTarget not supported by the debug server')],
	] as const) {
		test(`${what} falls back to Memory.get, remembered for the session`, async () => {
			const fake = fakeClient({ setLiveSet: () => { throw error; } });
			const logs: string[] = [];
			const evaluator = new LiveWatchEvaluator(fake.client, (m) => logs.push(m));
			const items = [{ expression: 'a' }, { expression: 'b' }];
			const first = await evaluator.evaluate('ctx', items, LIVE);
			assert.deepStrictEqual(first.map((r) => [r.available, r.value]), [[true, '0'], [true, '4']]);
			assert.strictEqual(fake.count(GetMemoryCommand), 2);
			assert.strictEqual(logs.length, 1);

			fake.reset();
			const second = await evaluator.evaluate('ctx', items, { ...LIVE, intervalMs: 2000 });
			assert.deepStrictEqual(second.map((r) => r.value), ['0', '4']);
			assert.deepStrictEqual(fake.sent.map((c) => (c as object).constructor.name), ['GetMemoryCommand', 'GetMemoryCommand']);
			// Nothing to clear either.
			await evaluator.stopLive('sidebar');
			assert.strictEqual(fake.count(ClearLiveSetAurixTargetCommand), 0);
		});
	}

	test('a region error is that root\'s error; a running error keeps the value shown', async () => {
		let answer: (regions: LiveRegion[]) => LiveValue[] = (regions) => regions.map((r) => (r.addr === 0x70000004
			? { error: 'Cannot read memory at 0x70000004: bus error' }
			: { data: memory(r.addr, r.size).toString('base64'), source: 'daq', tUs: 12 }));
		const fake = fakeClient({ getLiveValues: (regions) => answer(regions) });
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const items = [{ expression: 'a' }, { expression: 'b' }];
		const [a, b] = await evaluator.evaluate('ctx', items, LIVE);
		assert.strictEqual(a.available, true);
		assert.strictEqual(a.value, '0');
		assert.strictEqual(b.available, false);
		assert.strictEqual(b.error, 'Cannot read memory at 0x70000004: bus error');
		assert.strictEqual(b.stale, false);

		answer = (regions) => regions.map(() => ({ error: 'Execution context is running' }));
		const running = await evaluator.evaluate('ctx', items, LIVE);
		assert.deepStrictEqual(running.map((r) => [r.available, r.stale]), [[false, true], [false, true]]);
		// Neither error replaced the set or fell back to Memory.get.
		assert.strictEqual(fake.setLiveSets().length, 1);
		assert.strictEqual(fake.count(GetMemoryCommand), 0);
	});

	test('a getLiveValues dropped for a resume keeps every value shown, as dropped Memory.gets do', async () => {
		let drop = false;
		const fake = fakeClient({
			getLiveValues: (regions) => {
				if (drop) {
					// What the server's queue answers for a read a later resume took out.
					throw { Code: 90, Format: 'Execution context is running (read dropped for RunControl.resume)' };
				}
				return regions.map((r) => ({ data: memory(r.addr, r.size).toString('base64') }));
			},
		});
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const items = [{ expression: 'a' }, { expression: 'b' }];
		await evaluator.evaluate('ctx', items, LIVE);
		drop = true;
		const dropped = await evaluator.evaluate('ctx', items, LIVE);
		assert.deepStrictEqual(dropped.map((r) => [r.available, r.stale]), [[false, true], [false, true]]);
		assert.strictEqual(fake.count(GetMemoryCommand), 0, 'no reads right after the resume');
		drop = false;
		await evaluator.evaluate('ctx', items, LIVE);
		assert.strictEqual(fake.setLiveSets().length, 1, 'the set is still the server\'s');
	});

	test('a failed getLiveValues reads with Memory.get this time and sets the set again next time', async () => {
		let fail = true;
		const fake = fakeClient({
			getLiveValues: (regions) => {
				if (fail) {
					throw { Code: 1, Format: 'live set lost' };
				}
				return regions.map((r) => ({ data: memory(r.addr, r.size).toString('base64') }));
			},
		});
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const [a] = await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		assert.strictEqual(a.value, '0');
		assert.strictEqual(fake.count(GetMemoryCommand), 1);
		fail = false;
		await evaluator.evaluate('ctx', [{ expression: 'a' }], LIVE);
		assert.strictEqual(fake.setLiveSets().length, 2);
		assert.strictEqual(fake.count(GetMemoryCommand), 1);
	});

	test('peripheral registers and fields are raw regions of the set, read once per register', async () => {
		const fake = fakeClient({
			getLiveValues: (regions) => regions.map(() => ({ data: Buffer.from([0x34, 0x12, 0x00, 0x80]).toString('base64') })),
		});
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const register: LiveWatchRequestItem = { expression: '*(unsigned int*)0xf0000010', address: 0xf0000010, size: 4, hex: true };
		const items: LiveWatchRequestItem[] = [
			register,
			{ ...register, bitOffset: 4, bitWidth: 8 },
			{ ...register, bitOffset: 31, bitWidth: 1 },
			{ expression: 'a' },
		];
		const values = await evaluator.evaluate('S0.D0.C0', items, LIVE);
		assert.deepStrictEqual(values.map((r) => r.value), ['0x80001234', '0x23', '1', '2147488308']);
		assert.deepStrictEqual(fake.setLiveSets()[0].regions, [
			{ ctx: 'S0.D0.C0', addr: 0xf0000010, size: 4 },
			{ ctx: 'S0.D0.C0', addr: 0x70000000, size: 4 },
		]);
		// No expression for the registers.
		assert.deepStrictEqual(fake.sent.filter((c) => c instanceof CreateExpressionsCommand).map((c) => (c as CreateExpressionsCommand).expression), ['a']);

		// Read on their own (a one-off refresh), they are a Memory.get of the register.
		fake.reset();
		const [one] = await evaluator.evaluate('S0.D0.C0', [{ ...register, bitOffset: 0, bitWidth: 8 }]);
		assert.strictEqual(one.value, '0x10');
		assert.deepStrictEqual(fake.sent.map((c) => (c as object).constructor.name), ['GetMemoryCommand']);
	});

	test('an expression without an address is evaluated as before, outside the set', async () => {
		const fake = fakeClient();
		const evaluator = new LiveWatchEvaluator(fake.client, () => undefined);
		const [missing, a] = await evaluator.evaluate('ctx', [{ expression: 'nosuch' }, { expression: 'a' }], LIVE);
		assert.strictEqual(missing.available, false);
		assert.match(missing.error ?? '', /Undefined identifier nosuch/);
		assert.strictEqual(a.value, '0');
		assert.strictEqual(fake.setLiveSets()[0].regions.length, 1);
	});
});

suite('LiveWatchInstance live set', () => {
	type Request = { command: string; args: any };

	function fakeSession(requests: Request[]): vscode.DebugSession {
		return {
			type: 'aurixdbg',
			customRequest: async (command: string, args: any) => {
				requests.push({ command, args });
				if (command === 'aurix/liveWatchEvaluate') {
					return { values: args.items.map((i: any) => ({ name: i.expression, expression: i.expression, value: '1', hasChildren: false, available: true })) };
				}
				return undefined;
			},
		} as unknown as vscode.DebugSession;
	}

	function fakeContext(): vscode.ExtensionContext {
		const state = new Map<string, unknown>();
		return {
			extensionUri: vscode.Uri.file(__dirname),
			workspaceState: {
				get: (key: string, fallback?: unknown) => (state.has(key) ? state.get(key) : fallback),
				update: async (key: string, value: unknown) => void state.set(key, value),
			},
		} as unknown as vscode.ExtensionContext;
	}

	function fakeHost(visible: boolean) {
		return {
			visible,
			setDescription: () => undefined,
			webview: {
				options: {},
				html: '',
				cspSource: '',
				asWebviewUri: (u: vscode.Uri) => u,
				onDidReceiveMessage: () => ({ dispose: () => undefined }),
				postMessage: async () => true,
			},
		} as any;
	}

	async function until(condition: () => boolean, what: string): Promise<void> {
		const end = Date.now() + 3000;
		while (!condition()) {
			if (Date.now() > end) {
				assert.fail(`timed out waiting for ${what}`);
			}
			await new Promise((resolve) => setTimeout(resolve, 10));
		}
	}

	let instance: LiveWatchInstance | undefined;

	setup(() => {
		setLiveUpdateIntervalMs(1000);
		setLiveUpdateSessionActive(true);
	});

	teardown(() => {
		instance?.dispose();
		instance = undefined;
		setLiveUpdateSessionActive(false);
		setLiveUpdateIntervalMs(1000);
	});

	test('the periodic refresh sends the view and its interval; going inactive stops the set', async () => {
		const requests: Request[] = [];
		const session = fakeSession(requests);
		instance = new LiveWatchInstance(fakeContext(), { id: 'panel7', storageKey: 'test.liveSet', debugSession: () => session });
		const host = fakeHost(true);
		instance.attach(host);
		await instance.addPeripheral({ label: 'STM0.TIM0', address: 0xf0001010, sizeBits: 32, hasReadSideEffect: false });
		const periodic = () => requests.filter((r) => r.command === 'aurix/liveWatchEvaluate' && r.args.live === true);
		await until(() => periodic().length > 0, 'a periodic refresh');
		const first = periodic()[0].args;
		assert.strictEqual(first.viewId, 'panel7');
		assert.strictEqual(first.intervalMs, 1000);
		assert.deepStrictEqual(
			[first.items[0].address, first.items[0].size, first.items[0].hex],
			[0xf0001010, 4, true]);
		// Adding the expression read it once on its own, outside the live set.
		assert.ok(requests.some((r) => r.command === 'aurix/liveWatchEvaluate' && r.args.live === false));

		// The instance's own interval applies to the next refresh.
		instance.setIntervalOverride(500);
		await until(() => periodic().some((r) => r.args.intervalMs === 500), 'a refresh at 500 ms');

		// Hidden: the server drops the set.
		instance.setVisible(false);
		await until(() => requests.some((r) => r.command === 'aurix/liveWatchStop'), 'liveWatchStop');
		assert.deepStrictEqual(requests.filter((r) => r.command === 'aurix/liveWatchStop').map((r) => r.args), [{ viewId: 'panel7' }]);
		const stopAt = requests.findIndex((r) => r.command === 'aurix/liveWatchStop');
		await new Promise((resolve) => setTimeout(resolve, 100));
		assert.ok(!requests.slice(stopAt).some((r) => r.args?.live === true), 'no live refresh after the stop');

		// Shown again: the loop starts the set again; pausing stops it once more.
		instance.setVisible(true);
		await until(() => requests.slice(stopAt).some((r) => r.args?.live === true), 'a live refresh after showing');
		requests.splice(0);
		instance.dispose();
		instance = undefined;
		await until(() => requests.some((r) => r.command === 'aurix/liveWatchStop'), 'liveWatchStop on close');
	});
});
