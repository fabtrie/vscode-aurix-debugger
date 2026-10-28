import * as crypto from 'crypto';
import * as vscode from 'vscode';
import { LiveUpdater } from './liveUpdate';
import type { LiveWatchNode, LiveWatchRootResult } from './liveWatchEvaluator';
import { isLiveWatchPeripheral, LiveWatchPeripheral, peripheralExpression, PeripheralWatchEntry } from './peripheralLiveWatch';

type LiveWatchStoredExpression = {
	/** The C expression; for a peripheral register or field its label (e.g. "STM0.TIM0"). Unique. */
	expression: string;
	/** Excluded from the periodic refresh and shown in the "Paused" group. */
	paused?: boolean;
	/** Set for a register or field added from the XPeripherals view. */
	peripheral?: LiveWatchPeripheral;
};

/** One visible line of the view, as rendered by media/liveWatch.js. */
type LiveWatchRow = {
	id: string;
	depth: number;
	kind: 'group' | 'root' | 'child' | 'more';
	name: string;
	value: string;
	/** Colors the value like the Watch view does. */
	valueKind: 'number' | 'string' | 'boolean' | 'error' | 'other';
	hasChildren: boolean;
	expanded: boolean;
	paused: boolean;
	/** The value cannot be set here (a register field). */
	readOnly: boolean;
	/** The expression can be edited (not for peripheral registers and fields). */
	renamable: boolean;
	tooltip: string;
	/** data-vscode-context of the row, selects the context menu entries. */
	context: string;
};

type RowTarget = {
	root: string;
	expression: string;
	kind: LiveWatchRow['kind'];
	value: string;
	node?: LiveWatchNode;
};

/**
 * Argument of the Live Watch commands when run from a view's context menu: the row, and the Live
 * Watch instance it is in (the sidebar's when missing).
 */
export type LiveWatchCommandArgument = { rowId?: string; instanceId?: string } | undefined;

/** Where an instance is shown: the sidebar view or a panel. */
export interface LiveWatchHost {
	readonly webview: vscode.Webview;
	readonly visible: boolean;
	/** The refresh state ("1000 ms", "paused"): the view's description, or part of a panel's title. */
	setDescription(description: string): void;
}

export interface LiveWatchInstanceOptions {
	id: string;
	/** workspaceState key of the expression list. */
	storageKey: string;
	/** The instance's own refresh interval; the global one when missing. */
	intervalMs?: number;
	/** The webview got the focus, so it is the one "Add to Live Watch" offers first. */
	onDidFocus?: () => void;
}

const PAUSED_GROUP_ID = 'pausedGroup';

/**
 * One Live Watch: its own expression list and refresh interval, shown in the sidebar view or in a
 * panel (a host). Several can be open at once; each reads only while its host is on screen.
 */
export class LiveWatchInstance implements vscode.Disposable {
	readonly id: string;

	/** In the order they were added, like the Watch view. */
	private readonly expressions = new Map<string, LiveWatchStoredExpression>();
	private readonly results = new Map<string, LiveWatchRootResult>();
	/** Expanded node expressions per root expression. */
	private readonly expanded = new Map<string, Set<string>>();
	private pausedGroupExpanded = true;
	/** What each row shown in the webview refers to, by row id. */
	private rowTargets = new Map<string, RowTarget>();
	private lastPostedRows: string | undefined;
	private host: LiveWatchHost | undefined;
	private hostDisposables: vscode.Disposable[] = [];
	private selectedId: string | undefined;
	private refreshQueue: Promise<void> = Promise.resolve();
	/// The refresh loop, shared with the AUTOSAR OS views: one interval, and nothing read while
	/// the view is off screen.
	private readonly live = new LiveUpdater({
		// A halt refreshes everything, paused expressions included, the way the Watch view does;
		// the periodic loop only touches the ones that are not paused.
		refresh: onBreak => (onBreak ? this.refreshNow() : this.refreshRoots(this.liveRoots())),
		canRefresh: () => this.hasLiveExpressions(),
		onChanged: () => this.updateDescription(),
	});

	constructor(private readonly context: vscode.ExtensionContext, private readonly options: LiveWatchInstanceOptions) {
		this.id = options.id;
		this.loadExpressions();
		if (options.intervalMs !== undefined) {
			this.live.setIntervalOverride(options.intervalMs);
		}
	}

	/** Shows the instance in a view or panel. The host reports visibility changes with setVisible. */
	attach(host: LiveWatchHost): void {
		this.host = host;
		const mediaRoot = vscode.Uri.joinPath(this.context.extensionUri, 'media');
		host.webview.options = { enableScripts: true, localResourceRoots: [mediaRoot] };
		host.webview.html = this.getHtml(host.webview, mediaRoot);
		this.lastPostedRows = undefined;
		vscode.Disposable.from(...this.hostDisposables).dispose();
		this.hostDisposables = [host.webview.onDidReceiveMessage((message) => void this.onMessage(message))];
		// Visibility events only fire on a change, so the state it starts in has to be pushed once
		// here - otherwise a view that is already open never starts refreshing.
		this.live.setVisible(host.visible);
		this.updateDescription();
	}

	/** The host was closed. */
	detach(host: LiveWatchHost): void {
		if (this.host === host) {
			this.host = undefined;
			vscode.Disposable.from(...this.hostDisposables).dispose();
			this.hostDisposables = [];
			this.live.setVisible(false);
		}
	}

	setVisible(visible: boolean): void {
		this.live.setVisible(visible);
		if (visible) {
			this.render(true);
			// Paused expressions that never got a value (e.g. restored from the last session).
			const missing = Array.from(this.expressions.keys()).filter((e) => !this.results.has(e));
			if (missing.length > 0) {
				void this.refreshRoots(missing);
			}
		}
	}

	get visible(): boolean {
		return this.host?.visible === true;
	}

	/** The refresh interval in use, the instance's own or the global one. */
	get intervalMs(): number {
		return this.live.intervalMs;
	}

	/** The instance's own refresh interval, undefined when it follows the global one. */
	get intervalOverrideMs(): number | undefined {
		return this.live.intervalOverrideMs;
	}

	setIntervalOverride(ms: number | undefined): void {
		this.live.setIntervalOverride(ms);
	}

	dispose(): void {
		this.live.dispose();
		vscode.Disposable.from(...this.hostDisposables).dispose();
	}

	async addExpression(expression: string | undefined): Promise<void> {
		const value = expression?.trim();
		if (!value) {
			return;
		}
		this.expressions.set(value, { expression: value, paused: this.expressions.get(value)?.paused });
		await this.saveExpressions();
		this.changed();
		await this.refreshRoots([value]);
	}

	/** Adds a register or field from the XPeripherals view, shown under its peripheral path. */
	async addPeripheral(entry: PeripheralWatchEntry): Promise<void> {
		const { label, hasReadSideEffect: _, ...peripheral } = entry;
		const existing = this.expressions.get(label);
		this.expressions.set(label, { expression: label, paused: existing?.paused, peripheral });
		await this.saveExpressions();
		this.changed();
		await this.refreshRoots([label]);
	}

	async removeExpression(arg?: LiveWatchCommandArgument): Promise<void> {
		const target = this.targetOf(arg);
		if (target?.kind !== 'root') {
			return;
		}
		this.expressions.delete(target.root);
		this.results.delete(target.root);
		this.expanded.delete(target.root);
		await this.saveExpressions();
		this.changed();
	}

	/** Excludes an expression from the periodic refresh (moves it to the "Paused" group) or includes it again. */
	async setExpressionPaused(arg: LiveWatchCommandArgument, paused: boolean): Promise<void> {
		const target = this.targetOf(arg);
		const stored = target?.kind === 'root' ? this.expressions.get(target.root) : undefined;
		if (!target || !stored || Boolean(stored.paused) === paused) {
			return;
		}
		stored.paused = paused;
		await this.saveExpressions();
		this.changed();
		if (!paused) {
			await this.refreshRoots([target.root]);
		}
	}

	/** Starts editing the expression of a watched entry in the view. */
	async editExpression(arg?: LiveWatchCommandArgument): Promise<void> {
		const id = arg?.rowId ?? this.selectedId;
		const target = id ? this.rowTargets.get(id) : undefined;
		if (!id || target?.kind !== 'root' || this.expressions.get(target.root)?.peripheral) {
			return;
		}
		if (this.host?.visible) {
			void this.host.webview.postMessage({ type: 'edit', id, mode: 'expression' });
			return;
		}
		const expression = await vscode.window.showInputBox({
			title: 'Edit Live Watch Expression',
			value: target.root,
			ignoreFocusOut: true,
			validateInput: (input) => input.trim() === '' ? 'Expression is required.' : undefined,
		});
		if (expression !== undefined) {
			await this.renameExpression(id, expression);
		}
	}

	/** Replaces a watched expression (e.g. a typo), keeping its position and paused state. */
	private async renameExpression(id: string, expression: string): Promise<void> {
		const target = this.rowTargets.get(id);
		const next = expression.trim();
		if (target?.kind !== 'root' || next === '' || next === target.root) {
			return;
		}
		const old = this.expressions.get(target.root);
		if (!old || old.peripheral) {
			return;
		}
		const renamed = new Map<string, LiveWatchStoredExpression>();
		for (const [key, stored] of this.expressions) {
			if (key === target.root) {
				renamed.set(next, { expression: next, paused: old.paused });
			} else if (key !== next) {
				renamed.set(key, stored); // an existing entry with the new name is merged into this one
			}
		}
		this.expressions.clear();
		renamed.forEach((stored, key) => this.expressions.set(key, stored));
		this.results.delete(target.root);
		this.expanded.delete(target.root);
		await this.saveExpressions();
		this.changed();
		const newId = rootRowId(next, Boolean(old.paused));
		this.selectedId = newId;
		void this.host?.webview.postMessage({ type: 'select', id: newId });
		// Also evaluates a paused entry once, so the new expression shows a value.
		await this.refreshRoots([next]);
	}

	/** Starts editing the value in the view, like double-clicking a value in the Watch view. */
	async setValue(arg?: LiveWatchCommandArgument): Promise<void> {
		const id = arg?.rowId ?? this.selectedId;
		const target = id ? this.rowTargets.get(id) : undefined;
		if (!id || !target || (target.kind !== 'root' && target.kind !== 'child') || this.isReadOnly(target)) {
			return;
		}
		if (this.host?.visible) {
			void this.host.webview.postMessage({ type: 'edit', id });
			return;
		}
		const value = await vscode.window.showInputBox({
			title: `Set ${target.expression}`,
			value: target.node?.hasChildren ? '' : target.value,
			prompt: 'New value: decimal, 0x hex, 0b binary, a character or an enumerator name',
			ignoreFocusOut: true,
		});
		if (value !== undefined) {
			await this.writeValue(id, value);
		}
	}

	async copyValue(arg?: LiveWatchCommandArgument): Promise<void> {
		const target = this.targetOf(arg);
		if (target) {
			await vscode.env.clipboard.writeText(target.value);
		}
	}

	async copyExpression(arg?: LiveWatchCommandArgument): Promise<void> {
		const target = this.targetOf(arg);
		if (target?.expression) {
			await vscode.env.clipboard.writeText(target.expression);
		}
	}

	/**
	 * Sets a data breakpoint on the row's expression, meant as the Live Watch means it (`x@1` is
	 * core 1's copy). It shows up in the Breakpoints view as `expression [access]`.
	 */
	async addDataBreakpoint(arg: LiveWatchCommandArgument, access: 'write' | 'read' | 'readWrite'): Promise<void> {
		const target = this.targetOf(arg);
		if (!target?.expression) {
			return;
		}
		const session = vscode.debug.activeDebugSession;
		if (!session || session.type !== 'aurixdbg') {
			void vscode.window.showWarningMessage('Data breakpoints can only be set during an AURIX debug session.');
			return;
		}
		try {
			const r = await session.customRequest('aurix/liveWatchDataBreakpoint', { expression: target.expression, access }) as { name?: string } | undefined;
			vscode.window.setStatusBarMessage(`Data breakpoint ${r?.name ?? target.expression}`, 4000);
		} catch (e) {
			void vscode.window.showErrorMessage(e instanceof Error ? e.message : String(e));
		}
	}

	async clearExpressions(): Promise<void> {
		this.expressions.clear();
		this.results.clear();
		this.expanded.clear();
		await this.saveExpressions();
		this.changed();
	}

	/**
	 * Refreshes all expressions, including paused ones: the Refresh button, a new session and a break.
	 * Pausing (per expression or globally) only stops the periodic refresh while the target runs.
	 */
	async refreshNow(): Promise<void> {
		await this.refreshRoots(Array.from(this.expressions.keys()));
	}

	private async onMessage(message: any): Promise<void> {
		const id = typeof message?.id === 'string' ? message.id : undefined;
		switch (message?.type) {
			case 'ready':
				this.render(true);
				await this.refreshNow();
				break;
			case 'select':
				this.selectedId = id;
				break;
			case 'focus':
				this.options.onDidFocus?.();
				break;
			case 'toggle':
				if (id) {
					await this.toggle(id);
				}
				break;
			case 'rename':
				if (id) {
					await this.renameExpression(id, String(message.expression ?? ''));
				}
				break;
			case 'setValue':
				if (id) {
					await this.writeValue(id, String(message.value ?? ''));
				}
				break;
			case 'pause':
			case 'resume':
				await this.setExpressionPaused({ rowId: id }, message.type === 'pause');
				break;
			case 'remove':
				await this.removeExpression({ rowId: id });
				break;
		}
	}

	private async toggle(id: string): Promise<void> {
		if (id === PAUSED_GROUP_ID) {
			this.pausedGroupExpanded = !this.pausedGroupExpanded;
			this.render();
			return;
		}
		const target = this.rowTargets.get(id);
		if (!target?.node?.hasChildren) {
			return;
		}
		let set = this.expanded.get(target.root);
		if (!set) {
			set = new Set<string>();
			this.expanded.set(target.root, set);
		}
		if (set.delete(target.expression)) {
			this.render();
			return;
		}
		set.add(target.expression);
		this.render();
		if (!target.node.children) {
			// The last refresh did not include the children of a collapsed node.
			await this.refreshRoots([target.root]);
		}
	}

	private async writeValue(id: string, value: string): Promise<void> {
		const target = this.rowTargets.get(id);
		if (!target?.expression || this.isReadOnly(target)) {
			return;
		}
		const session = vscode.debug.activeDebugSession;
		if (!session || session.type !== 'aurixdbg') {
			void vscode.window.showWarningMessage('Live watch values can only be set during an AURIX debug session.');
			return;
		}
		try {
			await session.customRequest('aurix/liveWatchWrite', { expression: target.expression, value });
		} catch (e) {
			void vscode.window.showErrorMessage(`Could not set ${target.expression}: ${e instanceof Error ? e.message : String(e)}`);
		}
		// Also refreshes a paused expression, so the written value is shown.
		await this.refreshRoots([target.root]);
	}

	/** Refreshes are queued rather than dropped, so an added or expanded expression is always shown. */
	private refreshRoots(roots: string[]): Promise<void> {
		this.refreshQueue = this.refreshQueue.then(() => this.evaluateRoots(roots), () => this.evaluateRoots(roots));
		return this.refreshQueue;
	}

	private async evaluateRoots(roots: string[]): Promise<void> {
		roots = roots.filter((root) => this.expressions.has(root));
		const session = vscode.debug.activeDebugSession;
		if (roots.length === 0 || !session || session.type !== 'aurixdbg') {
			return;
		}
		const items = roots.map((root) => {
			const peripheral = this.expressions.get(root)?.peripheral;
			return peripheral
				? { expression: peripheralExpression(peripheral), bitOffset: peripheral.bitOffset, bitWidth: peripheral.bitWidth, hex: true }
				: { expression: root, expanded: Array.from(this.expanded.get(root) ?? []) };
		});
		let values: LiveWatchRootResult[];
		try {
			const response = await session.customRequest('aurix/liveWatchEvaluate', { items }) as { values?: LiveWatchRootResult[] } | undefined;
			values = response?.values ?? [];
		} catch (e) {
			const error = e instanceof Error ? e.message : String(e);
			values = items.map((item) => ({ name: item.expression, expression: item.expression, value: '', hasChildren: false, available: false, error }));
		}
		// One result per item, in order.
		roots.forEach((root, index) => {
			const value = values[index];
			if (value && this.expressions.has(root)) {
				this.results.set(root, { ...value, name: root });
			}
		});
		this.render();
	}

	/** Sends the visible rows to the webview; it only touches the rows that differ. */
	private render(force = false): void {
		if (!this.host) {
			return;
		}
		const rows = this.buildRows();
		const json = JSON.stringify(rows);
		if (!force && json === this.lastPostedRows) {
			return;
		}
		this.lastPostedRows = json;
		void this.host.webview.postMessage({
			type: 'rows',
			rows,
			emptyMessage: this.expressions.size === 0 ? 'Add expressions with + to watch them while the target is running.' : undefined,
		});
	}

	private buildRows(): LiveWatchRow[] {
		const rows: LiveWatchRow[] = [];
		this.rowTargets = new Map();
		const stored = Array.from(this.expressions.values());
		for (const e of stored.filter((s) => !s.paused)) {
			this.addRootRows(rows, e.expression, false, 0);
		}
		const paused = stored.filter((s) => s.paused);
		if (paused.length > 0) {
			rows.push({
				id: PAUSED_GROUP_ID,
				depth: 0,
				kind: 'group',
				name: 'Paused',
				value: 'updated on break',
				valueKind: 'other',
				hasChildren: true,
				expanded: this.pausedGroupExpanded,
				paused: true,
				readOnly: true,
				renamable: false,
				tooltip: 'Expressions excluded from the periodic Live Watch refresh. Like the Watch view, they are updated when the target stops.',
				context: JSON.stringify({ preventDefaultContextMenuItems: true }),
			});
			if (this.pausedGroupExpanded) {
				for (const e of paused) {
					this.addRootRows(rows, e.expression, true, 1);
				}
			}
		}
		return rows;
	}

	private addRootRows(rows: LiveWatchRow[], root: string, paused: boolean, depth: number): void {
		const result = this.results.get(root);
		const node = result ?? { name: root, expression: root, value: '', hasChildren: false };
		const error = !result ? '' : result.available ? undefined : result.error ?? 'not available';
		this.addNodeRows(rows, rootRowId(root, paused), depth, 'root', root, paused, node, error);
	}

	private isReadOnly(target: RowTarget): boolean {
		return target.kind === 'root' && this.expressions.get(target.root)?.peripheral?.bitWidth !== undefined;
	}

	private addNodeRows(
		rows: LiveWatchRow[], id: string, depth: number, kind: LiveWatchRow['kind'],
		root: string, paused: boolean, node: LiveWatchNode, error: string | undefined,
	): void {
		const expanded = node.hasChildren && this.expanded.get(root)?.has(node.expression) === true;
		const value = error ?? node.value;
		const peripheral = kind === 'root' ? this.expressions.get(root)?.peripheral : undefined;
		// Peripheral entries: "...register" (value can be set) or "...field" (read-only).
		const peripheralSection = !peripheral ? '' : peripheral.bitWidth !== undefined ? '.field' : '.register';
		const section = kind === 'root'
			? `liveWatch.root.${paused ? 'paused' : 'live'}${peripheralSection}`
			: kind === 'child' ? 'liveWatch.child' : undefined;
		const target: RowTarget = { root, expression: node.expression, kind, value, node };
		rows.push({
			id,
			depth,
			kind,
			name: node.name,
			value,
			valueKind: error !== undefined ? 'error' : classifyValue(node.value),
			hasChildren: node.hasChildren,
			expanded,
			paused,
			readOnly: this.isReadOnly(target),
			renamable: kind === 'root' && !peripheral,
			tooltip: kind === 'more' ? '' : tooltipOf(root, node, value, error, peripheral),
			context: JSON.stringify(section
				? { webviewSection: section, rowId: id, instanceId: this.id, preventDefaultContextMenuItems: true }
				: { preventDefaultContextMenuItems: true }),
		});
		this.rowTargets.set(id, target);
		if (expanded && node.children) {
			node.children.forEach((child, index) => {
				this.addNodeRows(rows, `${id}\n${child.expression || `#${index}`}`, depth + 1,
					child.expression ? 'child' : 'more', root, paused, child, undefined);
			});
		}
	}

	private targetOf(arg: LiveWatchCommandArgument): RowTarget | undefined {
		const id = arg?.rowId ?? this.selectedId;
		return id ? this.rowTargets.get(id) : undefined;
	}

	private liveRoots(): string[] {
		return Array.from(this.expressions.values()).filter((e) => !e.paused).map((e) => e.expression);
	}

	private hasLiveExpressions(): boolean {
		return Array.from(this.expressions.values()).some((e) => !e.paused);
	}

	/** The expression list changed. */
	private changed(): void {
		this.render();
		// Starts the loop if this added the first live expression; a running loop continues.
		this.live.kick();
	}

	private updateDescription(): void {
		this.host?.setDescription(this.live.description);
	}

	private getHtml(webview: vscode.Webview, mediaRoot: vscode.Uri): string {
		const nonce = crypto.randomBytes(16).toString('base64');
		const css = webview.asWebviewUri(vscode.Uri.joinPath(mediaRoot, 'liveWatch.css'));
		const js = webview.asWebviewUri(vscode.Uri.joinPath(mediaRoot, 'liveWatch.js'));
		return `<!DOCTYPE html>
<html lang="en">
<head>
	<meta charset="UTF-8">
	<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
	<meta name="viewport" content="width=device-width, initial-scale=1.0">
	<link href="${css}" rel="stylesheet">
</head>
<body data-vscode-context='{"preventDefaultContextMenuItems": true}' data-instance-id="${this.id}">
	<div id="tree" class="tree" role="tree" tabindex="0"></div>
	<div id="empty" class="empty" hidden></div>
	<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}

	private loadExpressions(): void {
		const stored = this.context.workspaceState.get<LiveWatchStoredExpression[]>(this.options.storageKey, []);
		for (const item of stored) {
			if (typeof item?.expression === 'string' && item.expression.trim() !== '') {
				this.expressions.set(item.expression, {
					expression: item.expression,
					paused: item.paused === true,
					peripheral: isLiveWatchPeripheral(item.peripheral) ? item.peripheral : undefined,
				});
			}
		}
	}

	private async saveExpressions(): Promise<void> {
		await this.context.workspaceState.update(this.options.storageKey, Array.from(this.expressions.values()));
	}
}

function tooltipOf(root: string, node: LiveWatchNode, value: string, error: string | undefined, peripheral: LiveWatchPeripheral | undefined): string {
	let heading = node.expression;
	if (peripheral) {
		const bits = peripheral.bitWidth === undefined
			? `${peripheral.sizeBits} bits`
			: peripheral.bitWidth === 1
				? `bit ${peripheral.bitOffset}`
				: `bits ${(peripheral.bitOffset ?? 0) + peripheral.bitWidth - 1}:${peripheral.bitOffset}`;
		heading = `${root} @ 0x${peripheral.address.toString(16)}, ${bits}`;
	}
	return error ? `${heading}\n${error}` : `${heading} = ${value}`;
}

function rootRowId(expression: string, paused: boolean): string {
	return `${paused ? 'paused' : 'live'}\n${expression}`;
}

/** Same classification the Watch view uses to color a value. */
function classifyValue(value: string): LiveWatchRow['valueKind'] {
	const v = value.trim();
	if (v === '') {
		return 'other';
	}
	if (v === 'true' || v === 'false') {
		return 'boolean';
	}
	if (v.startsWith('"') || v.startsWith('\'')) {
		return 'string';
	}
	return Number.isNaN(Number(v)) ? 'other' : 'number';
}
