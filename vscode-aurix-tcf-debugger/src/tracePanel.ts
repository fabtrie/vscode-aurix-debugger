/*
 * The Trace panel: configure, arm and read a miniMCDS program (or data) trace.
 *
 * The configuration form names what to record (all code, or only inside a function or address
 * range), when to stop (core halt, buffer full, manually) and the time
 * stamps; every change is checked by the server (`check`), which answers with the resources the
 * setup takes or why it cannot be armed. While armed the panel polls the status; a trace that
 * finished by itself (the core halted, the buffer filled) is read at once. The result is the
 * executed instructions grouped by function, with source lines and target times, gaps marked
 * (Flow), or every instruction with its source line, what it wrote and the registers rebuilt from
 * the halt (Instructions); a row can be shown in the debugger (trace replay).
 *
 * Configurations are saved per workspace by name.
 */

import * as path from 'path';
import * as vscode from 'vscode';
import type { PanelPlacement } from './debugLayout';
import type { TraceConfig } from './tcf/trace';

const CONFIGS_KEY = 'aurix-debugger.trace.configs';
const LAST_KEY = 'aurix-debugger.trace.last';
const POLL_MS = 500;

type Saved = { name: string; config: TraceConfig };

/**
 * The status bar item: "Trace: armed CPU0 · 62 % full" while a recording is armed or done, hidden
 * otherwise; a click opens the panel. Polls the status only while an AURIX session runs.
 */
export function registerTraceStatus(context: vscode.ExtensionContext): void {
	const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 97);
	item.command = 'aurix-debugger.showTrace';
	let timer: NodeJS.Timeout | undefined;
	let busy = false;
	const tick = async () => {
		const s = vscode.debug.activeDebugSession;
		if (busy || s?.type !== 'aurixdbg') {
			if (s?.type !== 'aurixdbg') {
				item.hide();
			}
			return;
		}
		busy = true;
		try {
			const st = await s.customRequest('aurix/trace', { op: 'status' });
			if (st?.Reverse) {
				// Reverse debugging owns the trace: its own status bar item shows it.
				item.hide();
			} else if (st?.Armed) {
				const core = st.Config?.Core ?? 0;
				const who = st.Owner === 'daq' ? ' (Live Watch)' : '';
				const fill = typeof st.Fill === 'number' ? ` · ${Math.round(st.Fill * 100)} % full` : '';
				item.text = st.Done ? `$(pulse) Trace: done CPU${core}${who}` : `$(pulse) Trace: armed CPU${core}${who}${fill}`;
				item.tooltip = fill
					? 'miniMCDS trace. How much of the 8 KB trace buffer holds data. A recording that stops on a halt, at a function or by hand keeps running at 100 %, overwriting the oldest data, and keeps the last 8 KB. Click to open the Trace panel.'
					: 'miniMCDS trace. Click to open the Trace panel.';
				item.show();
			} else if (st?.Auto) {
				item.text = '$(pulse) Trace: on run';
				item.tooltip = 'miniMCDS trace armed on every run and read at the halt (Step Back replays it). Click to open the Trace panel.';
				item.show();
			} else {
				item.hide();
			}
		} catch {
			item.hide();
		} finally {
			busy = false;
		}
	};
	const update = () => {
		const running = vscode.debug.activeDebugSession?.type === 'aurixdbg';
		if (running && !timer) {
			timer = setInterval(() => void tick(), 1500);
		} else if (!running && timer) {
			clearInterval(timer);
			timer = undefined;
			item.hide();
		}
	};
	context.subscriptions.push(
		item,
		vscode.debug.onDidChangeActiveDebugSession(update),
		vscode.debug.onDidTerminateDebugSession(update),
		{ dispose: () => timer && clearInterval(timer) },
	);
	update();
}

export class TracePanel {
	static readonly viewType = 'aurixTrace';
	private static current: TracePanel | undefined;

	private readonly disposables: vscode.Disposable[] = [];
	private config: TraceConfig;
	private poll: NodeJS.Timeout | undefined;
	private armed = false;
	private reading = false;
	private result: any;
	private shownLast = false;
	/** The recording last read (its stop time): a newer one, made by arming on run, is read too. */
	private lastRead: number | undefined;
	private auto = false;

	private constructor(
		private readonly panel: vscode.WebviewPanel,
		private readonly context: vscode.ExtensionContext,
		private readonly mediaRoot: vscode.Uri,
	) {
		this.config = context.workspaceState.get<TraceConfig>(LAST_KEY) ?? { Kind: 'program', Core: 0, Stop: 'halt', Timestamps: 'tsu', Ranges: [] };
		panel.webview.options = { enableScripts: true, localResourceRoots: [mediaRoot] };
		panel.webview.html = this.html(panel.webview);
		this.disposables.push(
			panel.webview.onDidReceiveMessage((m) => void this.onMessage(m)),
			vscode.debug.onDidTerminateDebugSession(() => {
				this.stopPolling();
				this.forgetResult();
			}),
			// A stop (or a reset seen as one): the shown recording may no longer end where the core is.
			vscode.debug.onDidChangeActiveStackItem(() => {
				if (this.result && !this.reading) {
					void this.status();
				}
			}),
			// A new session (another worker): read its state, not the last session's.
			vscode.debug.onDidStartDebugSession(() => {
				this.forgetResult();
				void this.status();
				void this.reverseRefresh();
			}),
		);
		panel.onDidDispose(() => this.dispose());
	}

	/** The debugger moved the trace replay (or left it): the Instructions view follows. */
	static replayMoved(active: boolean, at: number | undefined): void {
		TracePanel.current?.post({ type: 'replay', active, at });
	}

	/** Reverse debugging was switched on or off, or a stop changed its history. */
	static reverseChanged(): void {
		void TracePanel.current?.reverseRefresh();
	}

	/** Opens the panel on its History tab. */
	static showHistory(context: vscode.ExtensionContext): void {
		TracePanel.show(context);
		TracePanel.current?.post({ type: 'showTab', tab: 'history' });
		void TracePanel.current?.reverseRefresh();
	}

	private reverseOn = false;

	private async reverseRefresh(): Promise<void> {
		const s = this.session();
		if (!s) {
			this.reverseOn = false;
			this.post({ type: 'reverse', status: undefined, history: undefined });
			return;
		}
		try {
			const status = await s.customRequest('aurix/reverse', { op: 'status' });
			this.reverseOn = status?.Enabled === true;
			const history = this.reverseOn ? await s.customRequest('aurix/reverse', { op: 'history' }) : undefined;
			this.post({ type: 'reverse', status, history });
			if (this.reverseOn) {
				// The last action's recording, read as it comes (like arming on run).
				void this.status();
			}
		} catch {
			this.post({ type: 'reverse', status: undefined, history: undefined });
		}
	}

	static show(context: vscode.ExtensionContext, placement?: PanelPlacement): void {
		const media = vscode.Uri.joinPath(context.extensionUri, 'media');
		if (TracePanel.current) {
			TracePanel.current.panel.reveal(placement?.viewColumn, placement?.preserveFocus);
			return;
		}
		const column = placement?.viewColumn ?? vscode.ViewColumn.Active;
		const panel = vscode.window.createWebviewPanel(TracePanel.viewType, 'Trace', { viewColumn: column, preserveFocus: placement?.preserveFocus }, {
			enableScripts: true,
			retainContextWhenHidden: true,
			localResourceRoots: [media],
		});
		TracePanel.current = new TracePanel(panel, context, media);
	}

	dispose(): void {
		this.stopPolling();
		vscode.Disposable.from(...this.disposables).dispose();
		if (TracePanel.current === this) {
			TracePanel.current = undefined;
		}
	}

	private session(): vscode.DebugSession | undefined {
		const s = vscode.debug.activeDebugSession;
		return s?.type === 'aurixdbg' ? s : undefined;
	}

	private async request(op: string, extra: Record<string, unknown> = {}): Promise<any> {
		const s = this.session();
		if (!s) {
			throw new Error('No AURIX debug session');
		}
		return s.customRequest('aurix/trace', { op, ...extra });
	}

	private post(msg: Record<string, unknown>): void {
		void this.panel.webview.postMessage(msg);
	}

	private saved(): Saved[] {
		return this.context.workspaceState.get<Saved[]>(CONFIGS_KEY) ?? [];
	}

	private async onMessage(m: any): Promise<void> {
		switch (m?.type) {
			case 'ready':
				this.post({ type: 'init', config: this.config, saved: this.saved().map((s) => s.name) });
				void this.check();
				void this.status();
				void this.reverseRefresh();
				break;
			case 'reverseGoto': {
				// History tab ({index, row: 0}) or the Instructions tab ({row}: the last action).
				const s = this.session();
				try {
					if (!s) {
						throw new Error('No AURIX debug session');
					}
					let index = typeof m.index === 'number' ? m.index : undefined;
					if (index === undefined) {
						const h = await s.customRequest('aurix/reverse', { op: 'history' });
						const n = Array.isArray(h?.Checkpoints) ? h.Checkpoints.length : 0;
						index = h?.AtCheckpoint ? n - 2 : n - 1;
					}
					if (index === undefined || index < 0) {
						throw new Error('no history before this point');
					}
					await s.customRequest('aurix/reverse', { op: 'goto', index, row: m.row ?? 0 });
				} catch (e) {
					this.post({ type: 'error', error: e instanceof Error ? e.message : String(e) });
				}
				void this.reverseRefresh();
				break;
			}
			case 'config':
				this.config = m.config as TraceConfig;
				await this.context.workspaceState.update(LAST_KEY, this.config);
				await this.check();
				if (this.auto) {
					// Arming on run follows the form.
					await this.request('auto', { config: { ...this.config, Stop: 'halt' } }).catch(() => undefined);
				}
				break;
			case 'arm':
				await this.arm();
				break;
			case 'stop':
				await this.stopAndRead();
				break;
			case 'save': {
				const name = await vscode.window.showInputBox({ title: 'Save trace configuration', prompt: 'Name', value: m.name ?? '' });
				if (!name) {
					return;
				}
				const list = this.saved().filter((s) => s.name !== name);
				list.push({ name, config: this.config });
				await this.context.workspaceState.update(CONFIGS_KEY, list);
				this.post({ type: 'saved', saved: list.map((s) => s.name), selected: name });
				break;
			}
			case 'load': {
				const s = this.saved().find((x) => x.name === m.name);
				if (s) {
					this.config = s.config;
					await this.context.workspaceState.update(LAST_KEY, this.config);
					this.post({ type: 'init', config: this.config, saved: this.saved().map((x) => x.name), selected: s.name });
					await this.check();
				}
				break;
			}
			case 'open':
				await this.openSource(String(m.file ?? ''), Number(m.line ?? 0));
				break;
			case 'export':
				await this.exportCsv();
				break;
			case 'auto':
				try {
					// The form's configuration, stopping on the halt.
					const r = await this.request('auto', { config: m.on ? { ...this.config, Stop: 'halt' } : null });
					this.auto = Boolean(r?.Auto);
					this.post({ type: 'auto', on: this.auto });
					if (this.auto && !this.poll) {
						this.startPolling();
					}
				} catch (e) {
					this.post({ type: 'auto', on: this.auto, error: e instanceof Error ? e.message : String(e) });
				}
				break;
			case 'insns':
				await this.forward('insnsPage', 'insns', { from: m.from, count: m.count }, { from: m.from });
				break;
			case 'regs':
				await this.forward('regs', 'regs', { at: m.at }, { at: m.at });
				break;
			case 'find':
				await this.forward('found', 'find', { query: m.query, from: m.from, back: m.back }, { query: m.query });
				break;
			case 'sources':
				this.post({ type: 'sources', sources: await this.readSources((m.files ?? []) as string[]) });
				break;
			case 'replayAt':
			case 'replayOff': {
				const s = this.session();
				try {
					if (!s) {
						throw new Error('No AURIX debug session');
					}
					await s.customRequest('aurix/traceReplay', m.type === 'replayOff' ? { off: true } : { at: m.at });
				} catch (e) {
					this.post({ type: 'error', error: e instanceof Error ? e.message : String(e) });
				}
				break;
			}
		}
	}

	/** Sends a trace request and posts the answer to the webview as `type`. */
	private async forward(type: string, op: string, args: Record<string, unknown>, echo: Record<string, unknown>): Promise<void> {
		try {
			const r = await this.request(op, args);
			this.post({ type, ...echo, result: r });
		} catch (e) {
			this.post({ type, ...echo, error: e instanceof Error ? e.message : String(e) });
		}
	}

	/** The lines of the source files the trace shows (resolved like `openSource`), by path. */
	private async readSources(files: string[]): Promise<Record<string, string[]>> {
		const out: Record<string, string[]> = {};
		for (const file of files.slice(0, 200)) {
			try {
				const uri = await this.resolveSource(file);
				if (!uri) {
					continue;
				}
				const text = Buffer.from(await vscode.workspace.fs.readFile(uri)).toString('utf8');
				out[file] = text.split(/\r?\n/);
			} catch {
				// Not readable: rows show the location only.
			}
		}
		return out;
	}

	/** A source file as the debug information names it: that path, else a workspace file of that name. */
	private async resolveSource(file: string): Promise<vscode.Uri | undefined> {
		try {
			await vscode.workspace.fs.stat(vscode.Uri.file(file));
			return vscode.Uri.file(file);
		} catch {
			const found = await vscode.workspace.findFiles(`**/${path.basename(file.replace(/\\/g, '/'))}`, undefined, 2);
			return found[0];
		}
	}

	/** Asks the server what the configuration takes, or why it cannot be armed. */
	private async check(): Promise<void> {
		try {
			const r = await this.request('check', { config: this.config });
			this.post({ type: 'check', ok: true, check: r });
		} catch (e) {
			this.post({ type: 'check', ok: false, error: e instanceof Error ? e.message : String(e) });
		}
	}

	private async arm(): Promise<void> {
		try {
			const r = await this.request('arm', { config: this.config });
			this.armed = true;
			this.result = undefined;
			this.post({ type: 'armed', plan: r });
			this.startPolling();
		} catch (e) {
			this.post({ type: 'error', error: e instanceof Error ? e.message : String(e) });
		}
	}

	private startPolling(): void {
		this.stopPolling();
		this.poll = setInterval(() => void this.status(), POLL_MS);
	}

	private stopPolling(): void {
		if (this.poll) {
			clearInterval(this.poll);
			this.poll = undefined;
		}
	}

	/** Drops the shown recording: it belongs to a session that ended. */
	private forgetResult(): void {
		this.result = undefined;
		this.lastRead = undefined;
		this.shownLast = false;
		this.post({ type: 'result', result: undefined });
	}

	private async status(): Promise<void> {
		try {
			const st = await this.request('status');
			if (!st?.Last && this.result && !this.reading) {
				// The server has no recording (a new worker): what is shown is another session's.
				this.forgetResult();
			}
			this.armed = st?.Armed === true && st?.Owner === 'snapshot';
			// Reverse debugging records every action: its recordings are read as they come, too.
			const auto = Boolean(st?.Auto) || st?.Reverse === true;
			if (auto !== this.auto) {
				this.auto = auto;
				this.post({ type: 'auto', on: auto });
			}
			this.post({ type: 'status', status: st });
			const stopped = typeof st?.Last?.stopped_ms === 'number' ? st.Last.stopped_ms : undefined;
			if (this.armed && st?.Done === true && !this.reading) {
				// Finished by itself (the core halted, the buffer filled): read it now.
				await this.stopAndRead();
			} else if (auto && !this.armed && !this.reading && stopped !== undefined && stopped !== this.lastRead) {
				// Armed on run: the server read the recording at the halt; show it.
				await this.stopAndRead();
			} else if (this.armed || auto) {
				if (!this.poll) {
					this.startPolling();
				}
			} else {
				this.stopPolling();
				if (!this.result && st?.Last && !this.reading && !this.shownLast) {
					this.shownLast = true;
					// Opened after a recording was made: show it.
					await this.stopAndRead();
				}
			}
		} catch (e) {
			this.stopPolling();
			this.post({ type: 'status', status: undefined, error: e instanceof Error ? e.message : String(e) });
		}
	}

	private async stopAndRead(): Promise<void> {
		this.reading = true;
		this.stopPolling();
		try {
			if (this.armed) {
				await this.request('stop');
				this.armed = false;
			}
			this.post({ type: 'reading' });
			const st = await this.request('status');
			this.lastRead = typeof st?.Last?.stopped_ms === 'number' ? st.Last.stopped_ms : undefined;
			this.result = await this.request('read', { max: 20000 });
			this.post({ type: 'result', result: this.result });
		} catch (e) {
			this.post({ type: 'error', error: e instanceof Error ? e.message : String(e) });
		} finally {
			this.reading = false;
			void this.status();
		}
	}

	/** Opens a source line of the trace: the path as the debug information has it, else by name. */
	private async openSource(file: string, line: number): Promise<void> {
		if (!file) {
			return;
		}
		const uri = await this.resolveSource(file);
		if (!uri) {
			void vscode.window.showWarningMessage(`Source file not found: ${file}`);
			return;
		}
		const doc = await vscode.window.showTextDocument(uri, { viewColumn: vscode.ViewColumn.One, preserveFocus: true });
		const pos = new vscode.Position(Math.max(0, line - 1), 0);
		doc.selection = new vscode.Selection(pos, pos);
		doc.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenterIfOutsideViewport);
	}

	private async exportCsv(): Promise<void> {
		const r = this.result;
		if (!r) {
			return;
		}
		const target = await vscode.window.showSaveDialog({ filters: { CSV: ['csv'] }, saveLabel: 'Export trace' });
		if (!target) {
			return;
		}
		const lines: string[] = [];
		if (r.Kind === 'program') {
			lines.push('t_ns,address,function,file,line');
			for (const s of r.Steps ?? []) {
				if (Array.isArray(s)) {
					const [addr, t, fi, fl, li] = s;
					lines.push([t ?? '', '0x' + Number(addr).toString(16), r.Functions?.[fi] ?? '', r.Files?.[fl] ?? '', li ?? ''].join(','));
				} else {
					lines.push([s.T ?? '', '', `(gap: ${s.Gap})`, '', ''].join(','));
				}
			}
		} else {
			lines.push('t_ns,access,address,symbol,value,size');
			for (const e of r.Events ?? []) {
				lines.push([e.T ?? '', e.Access ?? (e.Lost ? 'lost' : ''), e.Addr !== undefined ? '0x' + Number(e.Addr).toString(16) : '', e.Symbol ?? '', e.Value ?? e.Lost ?? '', e.Size ?? ''].join(','));
			}
		}
		await vscode.workspace.fs.writeFile(target, Buffer.from(lines.join('\n') + '\n', 'utf8'));
	}

	private html(webview: vscode.Webview): string {
		const css = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'trace.css'));
		const js = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'trace.js'));
		const insnsJs = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'traceInsns.js'));
		const reverseJs = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'traceReverse.js'));
		const nonce = Math.random().toString(36).slice(2) + Date.now().toString(36);
		return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
<link rel="stylesheet" href="${css}">
<title>Trace</title>
</head>
<body>
<div id="toolbar">
  <span class="title">Trace</span>
  <select id="saved" title="Saved configurations"><option value="">(unsaved configuration)</option></select>
  <button id="save" type="button" class="secondary">Save…</button>
  <span class="spacer"></span>
  <span id="state" class="state idle">idle</span>
</div>
<div id="reverseBanner" class="banner" hidden>Trace used by reverse debugging: the setup and Arm are off while it is on. The tabs show the recording of the last action; History lists the checkpoints. Switch it off with <b>AURIX: Disable Reverse Debugging</b> to record here.</div>
<form id="form">
  <label>Core</label>
  <div><select id="core"><option value="0">CPU0</option><option value="1">CPU1</option><option value="2">CPU2</option></select></div>

  <label>Record</label>
  <div class="col">
    <label class="inline"><input type="radio" name="kind" value="all"> All code</label>
    <label class="inline"><input type="radio" name="kind" value="inside"> Only inside</label>
    <div class="sub"><input id="range" placeholder="function, or 0xLO..0xHI" spellcheck="false"><span id="rangeInfo" class="hint"></span></div>
    <label class="inline"><input type="radio" name="kind" value="data"> Every write into</label>
    <div class="sub"><input id="dataRange" placeholder="variable range: 0xADDR..0xEND, or function name" spellcheck="false"></div>
  </div>

  <label>Stop</label>
  <div><select id="stop">
    <option value="halt">When the core halts</option>
    <option value="full">When the buffer is full</option>
    <option value="manual">Manually</option>
    <option value="function">At a function</option>
  </select> <input id="stopAt" placeholder="function" spellcheck="false" hidden><span id="stopHint" class="hint"></span></div>

  <label>Keep</label>
  <div id="keep" class="hint"></div>

  <label>Time stamps</label>
  <div>
    <label class="inline"><input type="radio" name="ts" value="tsu"> With messages</label>
    <label class="inline"><input type="radio" name="ts" value="ticks"> Ticks (fills the buffer)</label>
  </div>
</form>
<div id="resources" class="resources"></div>
<div id="actions">
  <button id="arm" type="button">Arm</button>
  <button id="stopBtn" type="button" class="secondary">Stop and read</button>
  <label class="inline" id="autoLabel" title="Arm this trace whenever the core runs (Continue, not a step) and stop it when the core halts: every halt comes with the trace up to it, and Step Back replays it"><input id="auto" type="checkbox"> Arm on every run, stop on halt</label>
  <span class="spacer"></span>
  <button id="export" type="button" class="secondary" disabled>Export CSV</button>
</div>
<div id="message"></div>
<div id="tabs" hidden><button type="button" data-tab="flow" class="tab on">Flow</button><button type="button" data-tab="insns" class="tab">Instructions</button><button type="button" data-tab="history" class="tab" hidden>History</button></div>
<div id="history" hidden></div>
<div id="coverage"></div>
<div id="result"></div>
<div id="insns" hidden>
  <div id="insnSummary" class="summary"></div>
  <div id="insnToolbar">
    <input id="insnFind" type="search" placeholder="Find: address, instruction, function, d15=0xC, a4 changes" spellcheck="false">
    <button id="insnSrc" type="button" class="toggle on" title="Source lines between the instructions">Source</button>
    <button id="insnEff" type="button" class="toggle on" title="What each instruction wrote">Effects</button>
    <button id="insnCalls" type="button" class="toggle" title="Only calls, returns and interrupt entries">Calls only</button>
    <button id="insnStart" type="button" class="secondary" title="First instruction">⇤</button>
    <button id="insnEnd" type="button" class="secondary" title="Last instruction (the halt)">⇥</button>
    <span class="spacer"></span>
    <button id="insnReplay" type="button" title="Show the selected instruction in the debugger: source, registers, locals, call stack (Step Back and Step move on from there)">Show in debugger</button>
    <button id="insnReplayOff" type="button" class="secondary" hidden>Back to the halt</button>
  </div>
  <div id="insnMain">
    <div id="insnList" tabindex="0"><div id="insnSpacer"></div><div id="insnRows"></div></div>
    <div id="insnRegs"></div>
  </div>
  <div class="legend"><span><b class="v-exact">0x0000000C</b> exact</span><span><b class="v-mem">≈0x0000003C</b> from memory at the halt</span><span><b class="v-unk">········</b> unknown</span><span><b class="w">d15</b> written here</span></div>
</div>
<script nonce="${nonce}" src="${js}"></script>
<script nonce="${nonce}" src="${insnsJs}"></script>
<script nonce="${nonce}" src="${reverseJs}"></script>
</body>
</html>`;
	}
}
