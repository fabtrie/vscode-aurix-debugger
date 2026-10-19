import * as assert from 'assert';
import * as path from 'path';
import * as net from 'net';
import * as vscode from 'vscode';

function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

function getEnv(name: string): string | undefined {
	const v = process.env[name];
	return v && v.trim() !== '' ? v.trim() : undefined;
}

function getEnvList(name: string): string[] {
	return (getEnv(name) ?? '').split(';').map((x) => x.trim()).filter((x) => x !== '');
}

function getEnvNumber(name: string, fallback: number): number {
	const v = getEnv(name);
	if (!v) {
		return fallback;
	}
	const n = Number(v);
	return Number.isFinite(n) ? n : fallback;
}

function getDebugConfig(): vscode.DebugConfiguration | undefined {
	// Needs hardware: skipped unless AURIX_TEST_ELF is set.
	const program = getEnv('AURIX_TEST_ELF');
	if (!program) {
		return undefined;
	}
	const config: vscode.DebugConfiguration = {
		type: 'aurixdbg',
		request: 'launch',
		name: 'AURIX Stress',
		symbolFiles: [program],
		// aurix-dbg scripts, ';'-separated (OpenOCD -s / -f).
		searchDirs: getEnvList('AURIX_TEST_SEARCH_DIRS'),
		configFiles: getEnvList('AURIX_TEST_CONFIG_FILES'),
		host: '127.0.0.1',
		port: 1534,
		stopOnEntry: true,
		device: 'TC37x',
		workspace: getEnv('AURIX_TEST_WORKSPACE')
	};
	return config;
}

function canConnectTcp(host: string, port: number, timeoutMs: number): Promise<boolean> {
	return new Promise((resolve) => {
		const socket = new net.Socket();
		let done = false;
		const finish = (ok: boolean) => {
			if (done) {
				return;
			}
			done = true;
			try {
				socket.destroy();
			} catch {
				// ignore
			}
			resolve(ok);
		};

		socket.setTimeout(timeoutMs);
		socket.once('connect', () => finish(true));
		socket.once('timeout', () => finish(false));
		socket.once('error', () => finish(false));
		try {
			socket.connect(port, host);
		} catch {
			finish(false);
		}
	});
}

async function getThreads(session: vscode.DebugSession): Promise<Array<{ id: number; name?: string }>> {
	const resp = await session.customRequest('threads');
	const threads = Array.isArray(resp?.threads) ? resp.threads : [];
	return threads;
}

async function getTopFrame(session: vscode.DebugSession, threadId: number): Promise<any | undefined> {
	const resp = await session.customRequest('stackTrace', { threadId, startFrame: 0, levels: 1 });
	const frames = Array.isArray(resp?.stackFrames) ? resp.stackFrames : [];
	return frames[0];
}

async function waitForTopFrame(
	session: vscode.DebugSession,
	threadId: number,
	timeoutMs: number
): Promise<any> {
	const deadline = Date.now() + timeoutMs;
	let lastErr: unknown;
	while (Date.now() < deadline) {
		try {
			const frame = await getTopFrame(session, threadId);
			if (frame) {
				return frame;
			}
		} catch (e) {
			lastErr = e;
		}
		await sleep(500);
	}
	throw new Error(`Timed out waiting for stack frame: ${String(lastErr ?? '')}`);
}

suite('AURIX debug stress', () => {
	test('start debugger 10x and stop on core0 with expected file', async function () {
		const config = getDebugConfig();
		if (!config) {
			this.skip();
			return;
		}

		const runs = getEnvNumber('AURIX_TEST_RUNS', 10);
		const expectedFile = getEnv('AURIX_TEST_EXPECTED_SOURCE');
		const expectedEntry = getEnv('AURIX_TEST_ENTRY');
		const requireCpu0 = getEnv('AURIX_TEST_REQUIRE_CPU0') === '1';

		const folder = vscode.workspace.workspaceFolders?.[0];
		assert.ok(folder, 'No workspace folder open');

		// In the VS Code test host, activation events don't always fire in the order we expect.
		// Explicitly activate the extension so the debug adapter descriptor factory is registered.
		const ext = vscode.extensions.getExtension('fabtrie.aurix-debugger');
		assert.ok(ext, 'AURIX debugger extension not found in test host');
		await ext.activate();

		if (!config.autoStartAgent) {
			config.autoStartAgent = true;
		}
		// If auto-start is disabled, require an already-running agent to avoid a blocked dialog.
		if (!config.autoStartAgent) {
			const host = String((config as any).host ?? '127.0.0.1');
			const port = Number((config as any).port ?? 1534);
			const ok = await canConnectTcp(host, port, 300);
			if (!ok) {
				this.skip();
				return;
			}
		}

		for (let i = 0; i < runs; i++) {
			const started = await vscode.debug.startDebugging(folder, config);
			assert.ok(started, `Debug start failed at iteration ${i}`);

			const session = vscode.debug.activeDebugSession;
			assert.ok(session, `No active debug session at iteration ${i}`);

			// Give the target a moment to stop on entry before querying.
			await sleep(1500);

			const threads = await getThreads(session!);
			assert.ok(threads.length > 0, `No threads returned at iteration ${i}`);

			const cpu0 = threads.find((t) => /cpu0|core 0/i.test(t.name ?? ''));
			if (requireCpu0) {
				assert.ok(cpu0, `CPU0 thread not found at iteration ${i}`);
			}

			const threadId = (cpu0 ?? threads[0]).id;
			const frame = await waitForTopFrame(session!, threadId, 10000);

			if (expectedEntry) {
				const name = String(frame?.name ?? '');
				assert.ok(
					name.toLowerCase().includes(expectedEntry.toLowerCase()),
					`Expected top frame '${expectedEntry}', got '${name}' at iteration ${i}`
				);
			}

			if (expectedFile) {
				const framePath = String(frame?.source?.path ?? frame?.source?.name ?? '');
				const expectedBase = path.basename(expectedFile);
				assert.ok(
					framePath.includes(expectedBase),
					`Expected source '${expectedBase}', got '${framePath}' at iteration ${i}`
				);
			}

			await vscode.debug.stopDebugging(session!);
			await sleep(500);
		}
	});
});
