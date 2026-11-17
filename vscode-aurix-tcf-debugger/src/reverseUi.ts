/*
 * Reverse debugging and the trace replay in the editor and the status bar.
 *
 * The rule: the mode decides what Step Back does. With reverse debugging on it restores the real
 * core, which then looks like ordinary debugging (it is the live core, halted): a plain status bar
 * item "Reverse · 12". With it off, Step Back is the view-only trace replay, marked wherever the
 * state is shown: a warning status bar item, the current line in another colour with "view only,
 * the core is at file:line", and a gutter mark on the line where the core really halted.
 */

import * as path from 'path';
import * as vscode from 'vscode';
import { TracePanel } from './tracePanel';

let reverseItem: vscode.StatusBarItem | undefined;
let replayItem: vscode.StatusBarItem | undefined;
let replayLine: vscode.TextEditorDecorationType | undefined;
let haltLine: vscode.TextEditorDecorationType | undefined;
/** The view-only replay while one is shown: its position and where the core really halted. */
let replay: { at?: number; total?: number; haltFile?: string; haltLine?: number } | undefined;
let reverseOn = false;

function aurixSession(): vscode.DebugSession | undefined {
	const s = vscode.debug.activeDebugSession;
	return s?.type === 'aurixdbg' ? s : undefined;
}

export function registerReverseUi(context: vscode.ExtensionContext): void {
	reverseItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 96);
	reverseItem.name = 'AURIX Reverse Debugging';
	reverseItem.command = 'aurix-debugger.reverseHistory';
	replayItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 96);
	replayItem.name = 'AURIX Trace Replay';
	replayItem.command = 'aurix-debugger.traceReplayExit';
	replayItem.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground');
	replayLine = vscode.window.createTextEditorDecorationType({
		isWholeLine: true,
		backgroundColor: new vscode.ThemeColor('editor.inactiveSelectionBackground'),
		border: '1px dashed',
		borderColor: new vscode.ThemeColor('editorWarning.foreground'),
		after: { color: new vscode.ThemeColor('editorWarning.foreground'), margin: '0 0 0 2em', fontStyle: 'italic' },
	});
	haltLine = vscode.window.createTextEditorDecorationType({
		isWholeLine: true,
		overviewRulerColor: new vscode.ThemeColor('editorWarning.foreground'),
		overviewRulerLane: vscode.OverviewRulerLane.Left,
		after: { contentText: '◂ the core is halted here (live)', color: new vscode.ThemeColor('descriptionForeground'), margin: '0 0 0 2em' },
	});
	context.subscriptions.push(
		reverseItem,
		replayItem,
		replayLine,
		haltLine,
		vscode.commands.registerCommand('aurix-debugger.enableReverse', async () => {
			const s = aurixSession();
			if (!s) {
				return;
			}
			try {
				await s.customRequest('aurix/reverse', { op: 'enable' });
			} catch (e) {
				void vscode.window.showErrorMessage(`Reverse debugging not switched on: ${e instanceof Error ? e.message : String(e)}`);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.disableReverse', async () => {
			const s = aurixSession();
			if (s) {
				await s.customRequest('aurix/reverse', { op: 'disable' }).then(undefined, () => undefined);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.reverseHistory', () => TracePanel.showHistory(context)),
		vscode.debug.onDidChangeActiveStackItem(() => void decorate()),
		vscode.window.onDidChangeVisibleTextEditors(() => void decorate()),
		vscode.debug.onDidTerminateDebugSession(() => {
			if (!aurixSession()) {
				reverseStatus(undefined);
				traceReplay(undefined);
			}
		}),
	);
}

/** The adapter's `aurix/reverse` event: the mode's status (or off). */
export function reverseStatus(st: any): void {
	reverseOn = st?.Enabled === true;
	void vscode.commands.executeCommand('setContext', 'aurix.reverse', reverseOn);
	if (!reverseItem) {
		return;
	}
	if (!reverseOn) {
		reverseItem.hide();
	} else {
		const n = Number(st?.Checkpoints ?? 0);
		reverseItem.text = `$(history) Reverse · ${n}`;
		const kb = Math.round(Number(st?.SnapshotBytes ?? 0) / 1024);
		reverseItem.tooltip = `Reverse debugging on${st?.Target ? ` for ${st.Target}` : ''}: ${n} checkpoint${n === 1 ? '' : 's'}, ${kb} KB saved before every action${typeof st?.LastCostMs === 'number' ? ` (last: ${st.LastCostMs} ms)` : ''}. Step Back and Reverse Continue restore the target. Click for the history.`;
		reverseItem.show();
	}
	TracePanel.reverseChanged();
}

/** A stop: the history grew (or was cut). */
export function reverseStopped(): void {
	if (!reverseOn) {
		return;
	}
	const s = aurixSession();
	if (s) {
		void Promise.resolve(s.customRequest('aurix/reverse', { op: 'status' })).then(undefined, () => undefined);
	}
}

/** The adapter's `aurix/traceReplay` event: a view-only replay position, or the live core again. */
export function traceReplay(b: any): void {
	replay = b?.active ? { at: b.at, total: b.total, haltFile: b.haltFile, haltLine: b.haltLine } : undefined;
	if (replayItem) {
		if (replay) {
			const pos = typeof replay.at === 'number' ? ` ${replay.at + 1}/${replay.total ?? '?'}` : '';
			replayItem.text = `$(eye) Trace replay${pos} · view only`;
			replayItem.tooltip = 'The debugger shows a point of the trace, rebuilt from the recording: nothing on the target changed and the core is still at the halt. Click to go back to the live core. For a real step back enable "reverse" in the launch configuration.';
			replayItem.show();
		} else {
			replayItem.hide();
		}
	}
	void decorate();
}

async function sameFile(a: string, b: string): Promise<boolean> {
	const norm = (p: string) => path.normalize(p).replace(/\\/g, '/').toLowerCase();
	return norm(a) === norm(b) || norm(a).endsWith('/' + path.basename(norm(b)));
}

/** Marks the replayed line and the real halt line in the visible editors. */
async function decorate(): Promise<void> {
	if (!replayLine || !haltLine) {
		return;
	}
	const editors = vscode.window.visibleTextEditors;
	if (!replay) {
		for (const e of editors) {
			e.setDecorations(replayLine, []);
			e.setDecorations(haltLine, []);
		}
		return;
	}
	// The replayed line: the focused frame's source line.
	let cur: { path?: string; line?: number } | undefined;
	const item = vscode.debug.activeStackItem as vscode.DebugStackFrame | undefined;
	const s = aurixSession();
	if (s && item && typeof item.frameId === 'number') {
		cur = await Promise.resolve(s.customRequest('aurix/frameSource', { frameId: item.frameId })).then((r: any) => r, () => undefined);
	}
	const where = replay.haltFile ? `${path.basename(String(replay.haltFile))}:${replay.haltLine}` : 'the halt';
	for (const e of editors) {
		const file = e.document.uri.fsPath;
		const cr = cur?.path && cur.line && (await sameFile(file, cur.path))
			? [{ range: new vscode.Range(cur.line - 1, 0, cur.line - 1, 0), renderOptions: { after: { contentText: `trace replay: view only, the core is at ${where}` } } }]
			: [];
		e.setDecorations(replayLine, cr);
		const hr = replay.haltFile && replay.haltLine && (await sameFile(file, String(replay.haltFile)))
			? [new vscode.Range(replay.haltLine - 1, 0, replay.haltLine - 1, 0)]
			: [];
		e.setDecorations(haltLine, hr);
	}
}
