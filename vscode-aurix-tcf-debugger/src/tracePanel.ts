/*
 * The Trace panel: configure, arm and read a miniMCDS program (or data) trace.
 *
 * The configuration form names what to record (all code, or only inside a function or address
 * range), when to stop (core halt - stopped in hardware -, buffer full, manually) and the time
 * stamps; every change is checked by the server (`check`), which answers with the resources the
 * setup takes or why it cannot be armed. While armed the panel polls the status; a trace that
 * finished by itself (the core halted, the buffer filled) is read at once. The result is the
 * executed instructions grouped by function, with source lines and target times, gaps marked.
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
			if (st?.Armed) {
				const core = st.Config?.Core ?? 0;
				const who = st.Owner === 'daq' ? ' (Live Watch)' : '';
				const fill = typeof st.Fill === 'number' ? ` · ${Math.round(st.Fill * 100)} % full` : '';
				item.text = st.Done ? `$(pulse) Trace: done CPU${core}${who}` : `$(pulse) Trace: armed CPU${core}${who}${fill}`;
				item.tooltip = fill
					? 'miniMCDS trace. How much of the 8 KB trace buffer holds data. A recording that stops on a halt, at a function or by hand keeps running at 100 %, overwriting the oldest data, and keeps the last 8 KB. Click to open the Trace panel.'
					: 'miniMCDS trace. Click to open the Trace panel.';
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
			vscode.debug.onDidTerminateDebugSession(() => this.stopPolling()),
		);
		panel.onDidDispose(() => this.dispose());
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
				break;
			case 'config':
				this.config = m.config as TraceConfig;
				await this.context.workspaceState.update(LAST_KEY, this.config);
				await this.check();
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

	private async status(): Promise<void> {
		try {
			const st = await this.request('status');
			this.armed = st?.Armed === true && st?.Owner === 'snapshot';
			this.post({ type: 'status', status: st });
			if (this.armed && st?.Done === true && !this.reading) {
				// Finished by itself (the core halted, the buffer filled): read it now.
				await this.stopAndRead();
			} else if (this.armed) {
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
		let uri: vscode.Uri | undefined;
		try {
			await vscode.workspace.fs.stat(vscode.Uri.file(file));
			uri = vscode.Uri.file(file);
		} catch {
			const found = await vscode.workspace.findFiles(`**/${path.basename(file.replace(/\\/g, '/'))}`, undefined, 2);
			uri = found[0];
		}
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
  <span class="spacer"></span>
  <button id="export" type="button" class="secondary" disabled>Export CSV</button>
</div>
<div id="message"></div>
<div id="coverage"></div>
<div id="result"></div>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}
}
