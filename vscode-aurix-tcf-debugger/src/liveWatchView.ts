import * as vscode from 'vscode';
import type { LiveWatchNode, LiveWatchRootResult } from './liveWatchEvaluator';

type LiveWatchStoredExpression = {
	expression: string;
	/** Excluded from the periodic refresh and shown in the "Paused" group. */
	paused?: boolean;
};

type LiveWatchNodeElement = {
	kind: 'node';
	root: string;
	paused: boolean;
	node: LiveWatchNode;
	isRoot: boolean;
	/** Why a root expression has no value. */
	error?: string;
	/** Tree item id; unique per group, root and position. */
	id: string;
};

type LiveWatchElement = { kind: 'pausedGroup' } | LiveWatchNodeElement;

const LIVE_WATCH_EXPRESSIONS_KEY = 'aurix-tcf.liveWatch.expressions';
/** Minimum pause between two refreshes, so a slow target is not polled continuously. */
const MIN_REFRESH_GAP_MS = 50;

export class LiveWatchProvider implements vscode.TreeDataProvider<LiveWatchElement>, vscode.Disposable {
	private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<LiveWatchElement | LiveWatchElement[] | undefined | null | void>();
	readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

	/** In the order they were added, like the Watch view. */
	private readonly expressions = new Map<string, LiveWatchStoredExpression>();
	private readonly results = new Map<string, LiveWatchRootResult>();
	/** Expanded node expressions per root expression. */
	private readonly expanded = new Map<string, Set<string>>();
	/**
	 * Elements handed to the tree view, by id. A row gets the same object on every call, so a value
	 * change can refresh just that row; refreshing the whole tree makes VS Code re-create every row
	 * (flickering tooltips and inline buttons).
	 */
	private readonly elements = new Map<string, LiveWatchNodeElement>();
	private readonly pausedGroup: LiveWatchElement = { kind: 'pausedGroup' };
	private timer: NodeJS.Timeout | undefined;
	private refreshQueue: Promise<void> = Promise.resolve();
	private view: vscode.TreeView<LiveWatchElement> | undefined;
	private viewDisposables: vscode.Disposable[] = [];
	private selected: LiveWatchElement | undefined;
	private refreshEnabled = false;
	private refreshIntervalMs = 1000;

	constructor(private readonly context: vscode.ExtensionContext) {
		this.loadExpressions();
	}

	attachView(view: vscode.TreeView<LiveWatchElement>): void {
		this.view = view;
		vscode.Disposable.from(...this.viewDisposables).dispose();
		this.viewDisposables = [
			view.onDidChangeSelection((event) => {
				this.selected = event.selection[0];
			}),
			view.onDidExpandElement((event) => this.setExpanded(event.element, true)),
			view.onDidCollapseElement((event) => this.setExpanded(event.element, false)),
		];
		this.updateView();
	}

	dispose(): void {
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
		vscode.Disposable.from(...this.viewDisposables).dispose();
		this.onDidChangeTreeDataEmitter.dispose();
	}

	getTreeItem(element: LiveWatchElement): vscode.TreeItem {
		if (element.kind === 'pausedGroup') {
			const item = new vscode.TreeItem('Paused', vscode.TreeItemCollapsibleState.Expanded);
			item.id = 'aurixTcfLiveWatch.pausedGroup';
			item.description = 'not refreshed';
			item.iconPath = new vscode.ThemeIcon('debug-pause');
			item.tooltip = 'Expressions excluded from the periodic Live Watch refresh. Their last value is kept.';
			item.contextValue = 'aurixTcfLiveWatchPausedGroup';
			return item;
		}

		const { node, error } = element;
		const value = error === undefined ? node.value : 'not available';
		const label = `${node.name}: ${value}`;
		const state = !node.hasChildren
			? vscode.TreeItemCollapsibleState.None
			: this.expanded.get(element.root)?.has(node.expression)
				? vscode.TreeItemCollapsibleState.Expanded
				: vscode.TreeItemCollapsibleState.Collapsed;
		const item = new vscode.TreeItem({ label, highlights: [[0, node.name.length]] }, state);
		item.id = element.id;
		item.tooltip = error ? `${node.expression}\n${error}` : `${node.expression} = ${value}`;
		if (element.isRoot) {
			item.contextValue = element.paused ? 'aurixTcfLiveWatchExpression.paused' : 'aurixTcfLiveWatchExpression.live';
		} else if (node.expression !== '') {
			item.contextValue = 'aurixTcfLiveWatchChild';
		}
		return item;
	}

	async getChildren(element?: LiveWatchElement): Promise<LiveWatchElement[]> {
		if (!element) {
			const live = this.rootElements(false);
			return this.hasPausedExpressions() ? [...live, this.pausedGroup] : live;
		}
		if (element.kind === 'pausedGroup') {
			return this.rootElements(true);
		}
		if (element.node.hasChildren && !element.node.children) {
			// Expanded for the first time: the last refresh did not include the children yet.
			this.setExpanded(element, true);
			await this.refreshRoots([element.root]);
			if (!element.node.children) {
				element.node = findNode(this.results.get(element.root), element.node.expression) ?? element.node;
			}
		}
		return (element.node.children ?? []).map((child, index) =>
			this.getElement(childElementId(element, child, index), element.root, element.paused, false, child, undefined));
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

	async removeExpression(element?: LiveWatchElement): Promise<void> {
		const root = this.rootOf(element);
		if (!root) {
			return;
		}
		this.expressions.delete(root);
		this.results.delete(root);
		this.expanded.delete(root);
		this.selected = undefined;
		await this.saveExpressions();
		this.changed();
	}

	/** Excludes an expression from the periodic refresh (moves it to the "Paused" group) or includes it again. */
	async setExpressionPaused(element: LiveWatchElement | undefined, paused: boolean): Promise<void> {
		const root = this.rootOf(element);
		const stored = root ? this.expressions.get(root) : undefined;
		if (!root || !stored || Boolean(stored.paused) === paused) {
			return;
		}
		stored.paused = paused;
		await this.saveExpressions();
		this.changed();
		if (!paused) {
			await this.refreshRoots([root]);
		}
	}

	async setValue(element?: LiveWatchElement): Promise<void> {
		const target = element ?? this.selected;
		if (target?.kind !== 'node' || target.node.expression === '') {
			return;
		}
		const expression = target.node.expression;
		const session = vscode.debug.activeDebugSession;
		if (!session || session.type !== 'aurix-tcf') {
			void vscode.window.showWarningMessage('Live watch values can only be set during an AURIX TCF debug session.');
			return;
		}
		const value = await vscode.window.showInputBox({
			title: `Set ${expression}`,
			value: target.node.hasChildren ? '' : target.node.value,
			prompt: 'New value: decimal, 0x hex, 0b binary, a character or an enumerator name',
			ignoreFocusOut: true,
		});
		if (value === undefined) {
			return;
		}
		try {
			await session.customRequest('aurix-tcf/liveWatchWrite', { expression, value });
		} catch (e) {
			void vscode.window.showErrorMessage(`Could not set ${expression}: ${e instanceof Error ? e.message : String(e)}`);
		}
		// Also refreshes a paused expression, so the written value is shown.
		await this.refreshRoots([target.root]);
	}

	async clearExpressions(): Promise<void> {
		this.expressions.clear();
		this.results.clear();
		this.expanded.clear();
		this.selected = undefined;
		await this.saveExpressions();
		this.changed();
	}

	/** Refreshes the live expressions, and paused ones that have no value yet. */
	async refreshNow(): Promise<void> {
		const roots = Array.from(this.expressions.values())
			.filter((e) => !e.paused || !this.results.has(e.expression))
			.map((e) => e.expression);
		await this.refreshRoots(roots);
	}

	refreshScheduling(enabled: boolean, intervalMs: number): void {
		this.refreshEnabled = enabled;
		this.refreshIntervalMs = intervalMs;
		this.updateView();
		this.scheduleRefresh(0);
	}

	private scheduleRefresh(delayMs: number): void {
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
		if (!this.refreshEnabled || !this.hasLiveExpressions()) {
			return;
		}
		this.timer = setTimeout(() => {
			this.timer = undefined;
			void this.runScheduledRefresh();
		}, delayMs);
	}

	/**
	 * Runs one refresh and schedules the next one relative to its start, so updates come at a steady
	 * rate; a refresh that takes longer than the interval delays the next one instead of skipping it.
	 */
	private async runScheduledRefresh(): Promise<void> {
		const started = Date.now();
		await this.refreshRoots(this.liveRoots());
		if (!this.timer) {
			this.scheduleRefresh(Math.max(MIN_REFRESH_GAP_MS, this.refreshIntervalMs - (Date.now() - started)));
		}
	}

	/** Refreshes are queued rather than dropped, so an added or expanded expression is always shown. */
	private refreshRoots(roots: string[]): Promise<void> {
		this.refreshQueue = this.refreshQueue.then(() => this.evaluateRoots(roots), () => this.evaluateRoots(roots));
		return this.refreshQueue;
	}

	private async evaluateRoots(roots: string[]): Promise<void> {
		roots = roots.filter((root) => this.expressions.has(root));
		const session = vscode.debug.activeDebugSession;
		if (roots.length === 0 || !session || session.type !== 'aurix-tcf') {
			return;
		}
		const items = roots.map((expression) => ({ expression, expanded: Array.from(this.expanded.get(expression) ?? []) }));
		let values: LiveWatchRootResult[];
		try {
			const response = await session.customRequest('aurix-tcf/liveWatchEvaluate', { items }) as { values?: LiveWatchRootResult[] } | undefined;
			values = response?.values ?? [];
		} catch (e) {
			const error = e instanceof Error ? e.message : String(e);
			values = roots.map((expression) => ({ name: expression, expression, value: '', hasChildren: false, available: false, error }));
		}
		const changedElements: LiveWatchElement[] = [];
		for (const value of values) {
			const stored = this.expressions.get(value.expression);
			if (!stored) {
				continue; // removed while the request was running
			}
			const previous = this.results.get(value.expression);
			if (previous && JSON.stringify(previous) === JSON.stringify(value)) {
				continue;
			}
			this.results.set(value.expression, value);
			const rootElement = this.elements.get(rootElementId(value.expression, Boolean(stored.paused)));
			if (rootElement) {
				this.collectChangedElements(rootElement, value, rootError(value), changedElements);
			}
		}
		if (changedElements.length > 0) {
			this.onDidChangeTreeDataEmitter.fire(changedElements);
		}
	}

	/**
	 * Points the shown elements at the new result and collects the rows whose own display changed.
	 * Refreshing a row also reloads its children, so rows below a collected row are skipped.
	 */
	private collectChangedElements(element: LiveWatchNodeElement, node: LiveWatchNode, error: string | undefined, changed: LiveWatchElement[]): void {
		const previous = element.node;
		const previousError = element.error;
		element.node = node;
		element.error = error;
		if (previous.name !== node.name || previous.value !== node.value || previous.hasChildren !== node.hasChildren || previousError !== error) {
			changed.push(element);
			return;
		}
		// Children are only sent for expanded nodes; them appearing or disappearing needs no refresh.
		if (!previous.children || !node.children) {
			return;
		}
		const children = node.children;
		const sameChildren = previous.children.length === children.length
			&& previous.children.every((child, i) => child.name === children[i].name && child.expression === children[i].expression);
		if (!sameChildren) {
			changed.push(element);
			return;
		}
		children.forEach((child, index) => {
			const childElement = this.elements.get(childElementId(element, child, index));
			if (childElement) {
				this.collectChangedElements(childElement, child, undefined, changed);
			}
		});
	}

	private rootElements(paused: boolean): LiveWatchElement[] {
		return Array.from(this.expressions.values())
			.filter((e) => Boolean(e.paused) === paused)
			.map((e) => {
				const result = this.results.get(e.expression);
				const node = result ?? { name: e.expression, expression: e.expression, value: '', hasChildren: false };
				return this.getElement(rootElementId(e.expression, paused), e.expression, paused, true, node, rootError(result));
			});
	}

	/** Returns the element for a row: the same object as before if the row is shown already. */
	private getElement(id: string, root: string, paused: boolean, isRoot: boolean, node: LiveWatchNode, error: string | undefined): LiveWatchNodeElement {
		let element = this.elements.get(id);
		if (element) {
			element.node = node;
			element.error = error;
		} else {
			element = { kind: 'node', id, root, paused, isRoot, node, error };
			this.elements.set(id, element);
		}
		return element;
	}

	private rootOf(element: LiveWatchElement | undefined): string | undefined {
		const target = element ?? this.selected;
		return target?.kind === 'node' ? target.root : undefined;
	}

	private setExpanded(element: LiveWatchElement, expanded: boolean): void {
		if (element.kind !== 'node' || element.node.expression === '') {
			return;
		}
		let set = this.expanded.get(element.root);
		if (!set) {
			set = new Set<string>();
			this.expanded.set(element.root, set);
		}
		if (expanded) {
			set.add(element.node.expression);
		} else {
			set.delete(element.node.expression);
		}
	}

	private liveRoots(): string[] {
		return Array.from(this.expressions.values()).filter((e) => !e.paused).map((e) => e.expression);
	}

	private hasLiveExpressions(): boolean {
		return Array.from(this.expressions.values()).some((e) => !e.paused);
	}

	private hasPausedExpressions(): boolean {
		return Array.from(this.expressions.values()).some((e) => e.paused);
	}

	/** Tree structure or expression set changed: rebuilds the whole tree. */
	private changed(): void {
		this.elements.clear();
		this.updateView();
		if (!this.timer) {
			// Starts the refresh loop if this added the first live expression; a running loop continues.
			this.scheduleRefresh(this.refreshIntervalMs);
		}
		this.onDidChangeTreeDataEmitter.fire();
	}

	private updateView(): void {
		if (!this.view) {
			return;
		}
		this.view.message = this.expressions.size === 0 ? 'Add expressions to refresh while the target is running.' : undefined;
		this.view.description = this.refreshEnabled ? `${this.refreshIntervalMs} ms` : 'refresh off';
	}

	private loadExpressions(): void {
		const stored = this.context.workspaceState.get<LiveWatchStoredExpression[]>(LIVE_WATCH_EXPRESSIONS_KEY, []);
		for (const item of stored) {
			if (typeof item?.expression === 'string' && item.expression.trim() !== '') {
				this.expressions.set(item.expression, { expression: item.expression, paused: item.paused === true });
			}
		}
	}

	private async saveExpressions(): Promise<void> {
		await this.context.workspaceState.update(LIVE_WATCH_EXPRESSIONS_KEY, Array.from(this.expressions.values()));
	}
}

function rootElementId(expression: string, paused: boolean): string {
	return `${paused ? 'paused' : 'live'}\n${expression}`;
}

function childElementId(parent: LiveWatchNodeElement, child: LiveWatchNode, index: number): string {
	return `${parent.id}\n${child.expression || `#${index}`}`;
}

function rootError(result: LiveWatchRootResult | undefined): string | undefined {
	if (!result) {
		return 'no value yet';
	}
	return result.available ? undefined : result.error ?? 'not available';
}

function findNode(node: LiveWatchNode | undefined, expression: string): LiveWatchNode | undefined {
	if (!node || node.expression === expression) {
		return node;
	}
	for (const child of node.children ?? []) {
		const found = findNode(child, expression);
		if (found) {
			return found;
		}
	}
	return undefined;
}
