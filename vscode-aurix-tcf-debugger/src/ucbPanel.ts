/*
 * The "UCBs" panel: the TC3xx user configuration blocks of the active AURIX debug session.
 *
 * Everything comes from the debug server's `ucb` Tcl command (`-json`), run through the debug
 * adapter's `aurix/tcl` request: the readout, the staged changes (shared with the other clients,
 * e.g. changes Claude staged over MCP), the review and the write. The panel never decides what is
 * safe: the server's review does, and the server refuses a write without the session flag
 * ("ucbWrite"), without the token of the last review or, for permanent changes, without the
 * device name typed here.
 */

import * as vscode from 'vscode';

type Json = any;

/** Brace-quotes a Tcl word; values with braces or backslashes are refused. */
function tclWord(s: string): string {
	if (/[{}\\]/.test(s)) {
		throw new Error(`"${s}": braces and backslashes are not allowed`);
	}
	return `{${s}}`;
}

export class UcbPanel {
	static readonly viewType = 'aurixUcbs';
	private static current: UcbPanel | undefined;
	private readonly disposables: vscode.Disposable[] = [];
	private busy = false;

	private constructor(
		private readonly panel: vscode.WebviewPanel,
		private readonly mediaRoot: vscode.Uri,
	) {
		panel.webview.html = this.html(panel.webview);
		this.disposables.push(
			panel.webview.onDidReceiveMessage(m => void this.onMessage(m)),
			vscode.debug.onDidChangeActiveDebugSession(() => void this.load(false)),
			vscode.debug.onDidTerminateDebugSession(() => void this.load(false)),
		);
		panel.onDidDispose(() => this.dispose());
	}

	/** Opens (or reveals) the panel and reads the UCBs. */
	static show(extensionUri: vscode.Uri): UcbPanel {
		if (UcbPanel.current) {
			UcbPanel.current.panel.reveal(vscode.ViewColumn.Active);
			void UcbPanel.current.load(true);
			return UcbPanel.current;
		}
		const media = vscode.Uri.joinPath(extensionUri, 'media');
		const panel = vscode.window.createWebviewPanel(UcbPanel.viewType, 'UCBs', vscode.ViewColumn.Active, {
			enableScripts: true,
			retainContextWhenHidden: true,
			localResourceRoots: [media],
		});
		UcbPanel.current = new UcbPanel(panel, media);
		void UcbPanel.current.load(true);
		return UcbPanel.current;
	}

	/**
	 * After flashing: reviews the UCB records of the launch's flash files and, if they change
	 * UCBs, shows the review in the panel for the user to confirm. Silent when they hold none.
	 */
	static async reviewFlashImages(extensionUri: vscode.Uri, session: vscode.DebugSession): Promise<void> {
		let review: Json;
		try {
			review = await session.customRequest('aurix/ucbReviewImages', {});
		} catch (e) {
			void vscode.window.showErrorMessage(`AURIX™ Debugger: reviewing the UCB data of the flash files failed: ${e instanceof Error ? e.message : String(e)}`);
			return;
		}
		if (!review || !Array.isArray(review.changes) || review.changes.length === 0) {
			return;
		}
		const p = UcbPanel.show(extensionUri);
		await p.load(true);
		p.post({ type: 'review', review, source: 'images' });
		void vscode.window.showWarningMessage(
			`AURIX™ Debugger: the flash files change ${review.changes.length} UCB(s). They are not written until you confirm them in the UCBs panel.`,
		);
	}

	dispose(): void {
		vscode.Disposable.from(...this.disposables).dispose();
		if (UcbPanel.current === this) {
			UcbPanel.current = undefined;
		}
	}

	private session(): vscode.DebugSession | undefined {
		const s = vscode.debug.activeDebugSession;
		return s?.type === 'aurixdbg' ? s : undefined;
	}

	private post(m: Json): void {
		void this.panel.webview.postMessage(m);
	}

	/** Runs a Tcl command on the debug server; returns its result. */
	private async tcl(script: string, timeoutMs?: number): Promise<string> {
		const s = this.session();
		if (!s) {
			throw new Error('No active AURIX™ debug session');
		}
		const r = (await s.customRequest('aurix/tcl', { script, quiet: true, timeoutMs })) as { result?: string };
		return r?.result ?? '';
	}

	/** A `ucb ... -json` command: its JSON (the last line; log lines come first). */
	private async json(script: string, timeoutMs?: number): Promise<Json> {
		const out = await this.tcl(script, timeoutMs);
		const last = out.trim().split(/\r?\n/).pop() ?? '';
		return JSON.parse(last);
	}

	private async load(read: boolean, reveal?: string): Promise<void> {
		const s = this.session();
		if (!s) {
			this.post({ type: 'nosession' });
			return;
		}
		await this.run(read ? 'Reading the UCBs…' : undefined, async () => {
			const view = await this.json(`ucb ${read ? 'read' : 'show'} -json`);
			if (reveal) {
				const one = await this.json(`ucb show -json -reveal ${tclWord(reveal)}`);
				const u = one.ucbs?.[0];
				if (u) {
					view.ucbs = view.ucbs.map((x: Json) => (x.name === u.name ? u : x));
				}
			}
			this.post({ type: 'view', view, session: s.name });
		});
	}

	private async run(busyText: string | undefined, f: () => Promise<void>): Promise<void> {
		if (this.busy) {
			return;
		}
		this.busy = true;
		if (busyText) {
			this.post({ type: 'busy', text: busyText });
		}
		try {
			await f();
		} catch (e) {
			this.post({ type: 'error', message: e instanceof Error ? e.message : String(e) });
		} finally {
			this.busy = false;
			this.post({ type: 'busy', text: '' });
		}
	}

	private async onMessage(m: Json): Promise<void> {
		switch (m?.type) {
			case 'refresh':
				return this.load(true);
			case 'set':
				return this.run(undefined, async () => {
					await this.tcl(`ucb set ${tclWord(String(m.field))} ${tclWord(String(m.value))}`);
					await this.reload();
				});
			case 'discard':
				return this.run(undefined, async () => {
					await this.tcl(m.ucb ? `ucb unset ${tclWord(String(m.ucb))}` : 'ucb discard');
					await this.reload();
				});
			case 'reveal':
				return this.load(false, String(m.ucb));
			case 'password':
				return this.changePassword(String(m.ucb));
			case 'review':
				return this.run('Checking…', async () => {
					const review = await this.json('ucb review -json', 120000);
					this.post({ type: 'review', review, source: 'staged' });
				});
			case 'write':
				return this.write(String(m.token), typeof m.confirm === 'string' ? m.confirm : undefined);
			case 'export':
				return this.exportUcbs(m.selection ?? {});
			case 'compareOpen':
				return this.compareOpen();
			case 'compareAgain':
				return this.run('Comparing…', async () => {
					const files: string[] = Array.isArray(m.files) ? m.files.map(String) : [];
					await this.postCompare(await this.json(`ucb compare -json ${files.map(tclWord).join(' ')}`));
				});
			case 'apply':
			case 'revert':
				return this.run(undefined, async () => {
					await this.tcl(`ucb ${m.type} ${tclWord(String(m.target))}`);
					await this.postCompare(await this.json('ucb compare -json'));
				});
			case 'compareClose':
				return this.run(undefined, async () => {
					await this.tcl('ucb compare -close');
					this.post({ type: 'compareClosed' });
					await this.reload();
				});
		}
	}

	/** Picks image files and compares them with the device's UCBs. */
	private async compareOpen(): Promise<void> {
		const uris = await vscode.window.showOpenDialog({
			title: 'Compare the UCBs with image files',
			canSelectMany: true,
			filters: { 'Images with UCB records': ['hex', 'ihex', 's19', 'srec', 'mot', 'elf'], 'All files': ['*'] },
			defaultUri: vscode.workspace.workspaceFolders?.[0]?.uri,
		});
		if (!uris?.length) {
			return;
		}
		await this.run('Comparing…', async () => {
			const files = uris.map(u => u.fsPath.replace(/\\/g, '/'));
			await this.postCompare(await this.json(`ucb compare -json ${files.map(tclWord).join(' ')}`));
		});
	}

	/** The comparison and the readout with the staged changes (for the bottom bar). */
	private async postCompare(compare: Json): Promise<void> {
		this.post({ type: 'compare', compare });
		await this.reload();
	}

	private async reload(): Promise<void> {
		const s = this.session();
		const view = await this.json('ucb show -json');
		this.post({ type: 'view', view, session: s?.name });
	}

	/** Asks for a new password twice and stages it. */
	private async changePassword(ucb: string): Promise<void> {
		const ask = (prompt: string) =>
			vscode.window.showInputBox({
				title: `UCB_${ucb}: new password`,
				prompt,
				password: true,
				ignoreFocusOut: true,
				validateInput: v => (/^\s*(0x)?[0-9a-fA-F]{1,8}(\s+(0x)?[0-9a-fA-F]{1,8}){7}\s*$/.test(v) ? undefined : 'Eight 32-bit hex words, PW0 first'),
			});
		const a = await ask('PW0 … PW7 as eight hex words, PW0 first');
		if (a === undefined) {
			return;
		}
		const b = await ask('Enter the same password again');
		if (b === undefined) {
			return;
		}
		const norm = (s: string) => s.trim().split(/\s+/).map(w => parseInt(w, 16) >>> 0);
		if (norm(a).join() !== norm(b).join()) {
			void vscode.window.showErrorMessage('The two passwords differ: nothing was changed.');
			return;
		}
		await this.run(undefined, async () => {
			await this.tcl(`ucb set ${tclWord(`${ucb}.PW`)} ${norm(a).map(w => w.toString(16)).join(' ')}`);
			await this.reload();
		});
	}

	private async write(token: string, confirm?: string): Promise<void> {
		await this.run('Writing the UCBs: do not disconnect or power off the board…', async () => {
			const confirmArg = confirm ? ` -confirm ${tclWord(confirm)}` : '';
			const result = await this.json(`ucb write -json -token ${tclWord(token)}${confirmArg}`, 300000);
			this.post({ type: 'written', result });
			const view = await this.json('ucb show -json');
			this.post({ type: 'view', view, session: this.session()?.name });
			void vscode.window.showInformationMessage(`AURIX™ Debugger: ${result.written?.length ?? 0} UCB(s) written. Backup: ${result.backup ?? '-'}`);
		});
	}

	private async exportUcbs(sel: Json): Promise<void> {
		const args: string[] = [];
		if (Array.isArray(sel.ucbs) && sel.ucbs.length) {
			args.push(`-ucbs ${tclWord(sel.ucbs.join(' '))}`);
		}
		for (const st of Array.isArray(sel.states) ? sel.states : []) {
			args.push(`-state ${tclWord(String(st))}`);
		}
		if (Array.isArray(sel.classes) && sel.classes.length) {
			args.push(`-class ${tclWord(sel.classes.join(' '))}`);
		}
		if (sel.changed) {
			args.push('-changed');
		}
		if (sel.staged) {
			args.push('-staged');
		}
		if (typeof sel.which === 'string') {
			args.push(`-which ${tclWord(sel.which)}`);
		}
		await this.run('Exporting…', async () => {
			const r = await this.json(`ucb export - -json ${args.join(' ')}`);
			const uri = await vscode.window.showSaveDialog({
				title: `Export ${r.exported.length} UCB(s) to Intel HEX`,
				filters: { 'Intel HEX': ['hex'] },
				defaultUri: vscode.workspace.workspaceFolders?.[0] ? vscode.Uri.joinPath(vscode.workspace.workspaceFolders[0].uri, 'ucbs.hex') : undefined,
			});
			if (!uri) {
				return;
			}
			await vscode.workspace.fs.writeFile(uri, Buffer.from(r.hex, 'utf8'));
			this.post({ type: 'exported', result: { ...r, hex: undefined, file: uri.fsPath } });
			const notes = [r.skipped?.length ? `skipped: ${r.skipped.join('; ')}` : '', r.contains_passwords ? 'the file contains passwords' : ''].filter(Boolean);
			void vscode.window.showInformationMessage(`AURIX™ Debugger: exported ${r.exported.join(', ')} to ${uri.fsPath}${notes.length ? ` (${notes.join(', ')})` : ''}`);
		});
	}

	private html(webview: vscode.Webview): string {
		const css = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'ucbPanel.css'));
		const js = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'ucbPanel.js'));
		const nonce = Math.random().toString(36).slice(2) + Date.now().toString(36);
		return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
<link rel="stylesheet" href="${css}">
<title>UCBs</title>
</head>
<body>
<header>
  <h1>UCBs</h1>
  <span id="device" class="muted"></span>
  <span class="spacer"></span>
  <span id="busy" class="muted"></span>
  <button id="refresh" type="button">Read again</button>
  <button id="compareBtn" type="button">Compare with HEX…</button>
  <button id="exportBtn" type="button">Export…</button>
</header>
<div id="compareBanner" class="compare" hidden></div>
<div id="status" class="status"></div>
<div class="legend" id="compareLegend" hidden>
  <span><span class="dot file"></span>file differs from the device</span>
  <span><span class="dot staged"></span>taken from the file, not written yet</span>
  <span><span class="dot ifx"></span>cannot be applied</span>
</div>
<div class="legend" id="legend">
  <span><span class="dot changed"></span>changed from default</span>
  <span><span class="dot staged"></span>your change, not written yet</span>
  <span><span class="dot bad"></span>problem</span>
  <span><span class="dot ifx"></span>Infineon data</span>
</div>
<div id="error" class="msg bad" hidden></div>
<main>
  <div class="list">
    <div class="filter">
      <input type="search" id="ucbFilter" placeholder="Filter UCBs (name, field)…" autocomplete="off">
      <label><input type="checkbox" id="ucbChanged"> Only changed</label>
    </div>
    <div id="list"></div>
  </div>
  <div class="detail" id="detail"></div>
</main>
<div class="bar">
  <span id="stagedText"></span>
  <span class="spacer"></span>
  <span id="flagText" class="muted"></span>
  <button id="discard" type="button">Discard all</button>
  <button id="reviewBtn" class="primary" type="button">Review and write…</button>
</div>
<div class="modal-bg" id="modalBg" hidden><div class="modal" id="modal"></div></div>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}
}
