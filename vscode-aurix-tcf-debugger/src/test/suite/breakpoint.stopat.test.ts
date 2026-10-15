import * as assert from 'assert';
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

function getEnvNumber(name: string): number | undefined {
	const v = getEnv(name);
	if (!v) {
		return undefined;
	}
	const n = Number(v);
	return Number.isFinite(n) ? n : undefined;
}

function getDebugConfig(): vscode.DebugConfiguration | undefined {
	const program = getEnv('AURIX_TEST_ELF');
	if (!program) {
		return undefined;
	}
	const config: vscode.DebugConfiguration = {
		type: 'aurixdbg',
		request: 'launch',
		name: 'AURIX Breakpoint vs StopAtFunction',
		symbolFiles: [program],
		// aurix-dbg scripts, ';'-separated (OpenOCD -s / -f).
		searchDirs: getEnvList('AURIX_TEST_SEARCH_DIRS'),
		configFiles: getEnvList('AURIX_TEST_CONFIG_FILES'),
		host: '127.0.0.1',
		port: 1534,
		stopOnEntry: false,
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

suite('AURIX breakpoint vs stop-at-function', () => {
	test('breakpoint at core0_main before launch wins over stop-at-function', async function () {
		const config = getDebugConfig();
		if (!config) {
			this.skip();
			return;
		}

		const core0Source = getEnv('AURIX_TEST_CORE0_SOURCE');
		const core0Line = getEnvNumber('AURIX_TEST_CORE0_LINE');
		const core0Function = getEnv('AURIX_TEST_CORE0_FUNCTION') || 'core0_main';
		if (!core0Source || !core0Line || core0Line <= 0) {
			this.skip();
			return;
		}

		const folder = vscode.workspace.workspaceFolders?.[0];
		assert.ok(folder, 'No workspace folder open');

		const ext = vscode.extensions.getExtension('fabtrie.aurix-debugger');
		assert.ok(ext, 'AURIX debugger extension not found in test host');
		await ext.activate();

		if (!config.autoStartAgent) {
			config.autoStartAgent = true;
		}
		if (!config.autoStartAgent) {
			const host = String((config as any).host ?? '127.0.0.1');
			const port = Number((config as any).port ?? 1534);
			const ok = await canConnectTcp(host, port, 300);
			if (!ok) {
				this.skip();
				return;
			}
		}

		const existing = vscode.debug.breakpoints;
		if (existing.length) {
			vscode.debug.removeBreakpoints(existing);
		}

		const bpLocation = new vscode.Location(
			vscode.Uri.file(core0Source),
			new vscode.Position(core0Line - 1, 0)
		);
		const bp = new vscode.SourceBreakpoint(bpLocation);
		vscode.debug.addBreakpoints([bp]);

		try {
			const started = await vscode.debug.startDebugging(folder, config);
			assert.ok(started, 'Debug start failed');

			const session = vscode.debug.activeDebugSession;
			assert.ok(session, 'No active debug session');

			await sleep(1500);

			const threads = await getThreads(session!);
			assert.ok(threads.length > 0, 'No threads returned');

			const cpu0 = threads.find((t) => /cpu0|core 0/i.test(t.name ?? ''));
			const threadId = (cpu0 ?? threads[0]).id;
			const frame = await waitForTopFrame(session!, threadId, 20000);
			const name = String(frame?.name ?? '');

			assert.ok(
				name.toLowerCase().includes(core0Function.toLowerCase()),
				`Expected top frame '${core0Function}', got '${name}'`
			);
		} finally {
			vscode.debug.removeBreakpoints([bp]);
			const session = vscode.debug.activeDebugSession;
			if (session) {
				await vscode.debug.stopDebugging(session);
				await sleep(500);
			}
		}
	});
});
