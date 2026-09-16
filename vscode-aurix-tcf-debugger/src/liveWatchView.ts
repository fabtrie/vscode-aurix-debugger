import * as vscode from 'vscode';

type LiveWatchStoredExpression = {
	expression: string;
};

type LiveWatchEvalResult = {
	expression: string;
	result: string;
	available: boolean;
	error?: string;
};

const LIVE_WATCH_EXPRESSIONS_KEY = 'aurix-tcf.liveWatch.expressions';

class LiveWatchItem extends vscode.TreeItem {
	constructor(public readonly expression: string, result?: LiveWatchEvalResult) {
		super(expression, vscode.TreeItemCollapsibleState.None);
		this.id = expression;
		this.contextValue = 'aurixTcfLiveWatchExpression';
		this.iconPath = new vscode.ThemeIcon('eye');
		this.description = result?.available ? result.result : 'not available';
		this.tooltip = result?.available
			? `${expression} = ${result.result}`
			: result?.error
				? `${expression}\n${result.error}`
				: expression;
	}
}

export class LiveWatchProvider implements vscode.TreeDataProvider<LiveWatchItem>, vscode.Disposable {
	private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<LiveWatchItem | undefined | null | void>();
	readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

	private readonly expressions = new Map<string, LiveWatchStoredExpression>();
	private readonly results = new Map<string, LiveWatchEvalResult>();
	private timer: NodeJS.Timeout | undefined;
	private refreshInFlight = false;
	private view: vscode.TreeView<LiveWatchItem> | undefined;
	private viewSelectionDisposable: vscode.Disposable | undefined;
	private selectedExpression: string | undefined;
	private refreshEnabled = false;
	private refreshIntervalMs = 1000;

	constructor(private readonly context: vscode.ExtensionContext) {
		this.loadExpressions();
	}

	attachView(view: vscode.TreeView<LiveWatchItem>): void {
		this.view = view;
		this.viewSelectionDisposable?.dispose();
		this.viewSelectionDisposable = view.onDidChangeSelection((event) => {
			this.selectedExpression = event.selection[0]?.expression;
		});
		this.updateViewMessage();
	}

	dispose(): void {
		if (this.timer) {
			clearInterval(this.timer);
			this.timer = undefined;
		}
		this.viewSelectionDisposable?.dispose();
		this.onDidChangeTreeDataEmitter.dispose();
	}

	getTreeItem(element: LiveWatchItem): vscode.TreeItem {
		return element;
	}

	getChildren(element?: LiveWatchItem): Thenable<LiveWatchItem[]> {
		if (element) {
			return Promise.resolve([]);
		}
		const items = Array.from(this.expressions.keys())
			.sort((a, b) => a.localeCompare(b))
			.map((expression) => new LiveWatchItem(expression, this.results.get(expression)));
		return Promise.resolve(items);
	}

	async addExpression(expression?: string): Promise<void> {
		const value = (expression ?? await vscode.window.showInputBox({
			title: 'Add Live Watch Expression',
			placeHolder: 'Enter an expression, e.g. counter_6 or *(uint32_t*)0x70000044',
			ignoreFocusOut: true,
			validateInput: (input) => input.trim() === '' ? 'Expression is required.' : undefined,
		}))?.trim();
		if (!value) {
			return;
		}
		this.expressions.set(value, { expression: value });
		await this.saveExpressions();
		this.applyRefreshScheduling();
		this.updateViewMessage();
		this.onDidChangeTreeDataEmitter.fire();
		await this.refreshNow();
	}

	async removeExpression(item?: LiveWatchItem): Promise<void> {
		const expression = item?.expression ?? this.selectedExpression;
		if (!expression) {
			return;
		}
		this.expressions.delete(expression);
		this.results.delete(expression);
		if (this.selectedExpression === expression) {
			this.selectedExpression = undefined;
		}
		await this.saveExpressions();
		this.applyRefreshScheduling();
		this.updateViewMessage();
		this.onDidChangeTreeDataEmitter.fire();
	}

	async setValue(item?: LiveWatchItem): Promise<void> {
		const expression = item?.expression ?? this.selectedExpression;
		if (!expression) {
			return;
		}
		const session = vscode.debug.activeDebugSession;
		if (!session || session.type !== 'aurix-tcf') {
			void vscode.window.showWarningMessage('Live watch values can only be set during an AURIX TCF debug session.');
			return;
		}
		const current = this.results.get(expression);
		const value = await vscode.window.showInputBox({
			title: `Set ${expression}`,
			value: current?.available ? current.result : '',
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
		await this.refreshNow();
	}

	async clearExpressions(): Promise<void> {
		this.expressions.clear();
		this.results.clear();
		this.selectedExpression = undefined;
		await this.saveExpressions();
		this.applyRefreshScheduling();
		this.updateViewMessage();
		this.onDidChangeTreeDataEmitter.fire();
	}

	async refreshNow(): Promise<void> {
		await this.refreshValues();
	}

	refreshScheduling(enabled: boolean, intervalMs: number): void {
		this.refreshEnabled = enabled;
		this.refreshIntervalMs = intervalMs;
		this.applyRefreshScheduling();
	}

	private applyRefreshScheduling(): void {
		if (this.timer) {
			clearInterval(this.timer);
			this.timer = undefined;
		}
		if (!this.refreshEnabled || this.expressions.size === 0) {
			this.updateViewMessage();
			return;
		}
		this.timer = setInterval(() => {
			void this.refreshValues();
		}, this.refreshIntervalMs);
		this.updateViewMessage();
	}

	private updateViewMessage(): void {
		if (!this.view) {
			return;
		}
		if (this.expressions.size === 0) {
			this.view.message = 'Add expressions to refresh while the target is running.';
			return;
		}
		this.view.message = undefined;
	}

	private async refreshValues(): Promise<void> {
		if (this.refreshInFlight || this.expressions.size === 0) {
			return;
		}
		const session = vscode.debug.activeDebugSession;
		if (!session || session.type !== 'aurix-tcf') {
			return;
		}
		this.refreshInFlight = true;
		try {
			const expressions = Array.from(this.expressions.keys());
			const response = await session.customRequest('aurix-tcf/liveWatchEvaluate', { expressions }) as { values?: LiveWatchEvalResult[] } | undefined;
			let changed = false;
			for (const value of response?.values ?? []) {
				const previous = this.results.get(value.expression);
				if (!previous || previous.result !== value.result || previous.available !== value.available || previous.error !== value.error) {
					changed = true;
				}
				this.results.set(value.expression, value);
			}
			if (changed) {
				this.onDidChangeTreeDataEmitter.fire();
			}
		} catch (e) {
			const message = e instanceof Error ? e.message : String(e);
			let changed = false;
			for (const expression of this.expressions.keys()) {
				const nextValue = {
					expression,
					result: '',
					available: false,
					error: message,
				};
				const previous = this.results.get(expression);
				if (!previous || previous.result !== nextValue.result || previous.available !== nextValue.available || previous.error !== nextValue.error) {
					changed = true;
				}
				this.results.set(expression, nextValue);
			}
			if (changed) {
				this.onDidChangeTreeDataEmitter.fire();
			}
		} finally {
			this.refreshInFlight = false;
		}
	}

	private loadExpressions(): void {
		const stored = this.context.workspaceState.get<LiveWatchStoredExpression[]>(LIVE_WATCH_EXPRESSIONS_KEY, []);
		for (const item of stored) {
			if (typeof item?.expression === 'string' && item.expression.trim() !== '') {
				this.expressions.set(item.expression, { expression: item.expression });
			}
		}
	}

	private async saveExpressions(): Promise<void> {
		await this.context.workspaceState.update(LIVE_WATCH_EXPRESSIONS_KEY, Array.from(this.expressions.values()));
	}
}