/*
 * The Live Watch instances: the one in the sidebar, and any number of named panels.
 *
 * Each instance has its own expressions and, optionally, its own refresh interval. A panel is an
 * editor, so it can be dragged into a floating window (VS Code 1.85+) to make room while debugging.
 * Closing a panel keeps its expressions: it can be reopened by name, and only deleting removes it.
 */
import * as crypto from 'crypto';
import * as vscode from 'vscode';
import { liveUpdateIntervalMs, normalizeIntervalMs } from './liveUpdate';
import { LiveWatchCommandArgument, LiveWatchHost, LiveWatchInstance } from './liveWatchView';

const SIDEBAR_ID = 'sidebar';
/** The sidebar keeps the key the single Live Watch always used, so its expressions survive. */
const SIDEBAR_EXPRESSIONS_KEY = 'aurix-debugger.liveWatch.expressions';
const SIDEBAR_INTERVAL_KEY = 'aurix-debugger.liveWatch.sidebarIntervalMs';
const PANELS_KEY = 'aurix-debugger.liveWatch.panels';
const INTERVAL_OPTIONS = [200, 500, 1000, 2000];

/** A named panel's settings; its expressions are stored under expressionsKey(id). */
type SavedPanel = { id: string; name: string; intervalMs?: number };

function expressionsKey(id: string): string {
	return `${SIDEBAR_EXPRESSIONS_KEY}.${id}`;
}

function isSavedPanel(value: unknown): value is SavedPanel {
	const v = value as SavedPanel | undefined;
	return typeof v?.id === 'string' && /^[0-9a-f]+$/.test(v.id) && typeof v.name === 'string' && v.name.trim() !== '';
}

export class LiveWatchManager implements vscode.Disposable {
	static readonly viewType = 'aurixLiveWatch';
	static readonly panelViewType = 'aurixLiveWatchPanel';

	readonly sidebar: LiveWatchInstance;
	/** The open panels, by saved panel id. */
	private readonly panels = new Map<string, LiveWatchPanel>();
	private saved: SavedPanel[];
	/** The instance used last, offered first by "Add to Live Watch". */
	private lastUsedId = SIDEBAR_ID;
	private readonly disposables: vscode.Disposable[] = [];

	constructor(private readonly context: vscode.ExtensionContext) {
		const stored = context.workspaceState.get<unknown[]>(PANELS_KEY, []);
		this.saved = (Array.isArray(stored) ? stored : []).filter(isSavedPanel)
			.map((p) => ({ id: p.id, name: p.name, intervalMs: typeof p.intervalMs === 'number' ? normalizeIntervalMs(p.intervalMs) : undefined }));
		const sidebarInterval = context.workspaceState.get<number>(SIDEBAR_INTERVAL_KEY);
		this.sidebar = new LiveWatchInstance(context, {
			id: SIDEBAR_ID,
			storageKey: SIDEBAR_EXPRESSIONS_KEY,
			intervalMs: typeof sidebarInterval === 'number' ? sidebarInterval : undefined,
			onDidFocus: () => (this.lastUsedId = SIDEBAR_ID),
		});
		this.disposables.push(
			vscode.window.registerWebviewViewProvider(LiveWatchManager.viewType, new LiveWatchSidebarProvider(this.sidebar)),
			// Panels open when the window closed come back after a reload, also in floating windows.
			vscode.window.registerWebviewPanelSerializer(LiveWatchManager.panelViewType, {
				deserializeWebviewPanel: async (panel, state: { instanceId?: unknown } | undefined) => this.restorePanel(panel, state?.instanceId),
			}),
		);
	}

	dispose(): void {
		// Stops the refresh loops only: closing the panels here would keep VS Code from restoring
		// them after a reload.
		for (const panel of this.panels.values()) {
			panel.instance.dispose();
		}
		this.sidebar.dispose();
		vscode.Disposable.from(...this.disposables).dispose();
	}

	/** The instance a command acts on: the one of the context menu's row, else the sidebar's. */
	resolve(arg?: LiveWatchCommandArgument): LiveWatchInstance {
		const id = arg?.instanceId;
		return (id && this.panels.get(id)?.instance) || this.sidebar;
	}

	/** Refreshes every Live Watch, paused expressions included: a session started or changed. */
	async refreshAll(): Promise<void> {
		await Promise.all([this.sidebar, ...Array.from(this.panels.values()).map((p) => p.instance)].map((i) => i.refreshNow()));
	}

	/** The panel whose editor is active, for the commands in a panel's title bar. */
	activePanelInstance(): LiveWatchInstance | undefined {
		return Array.from(this.panels.values()).find((p) => p.panel.active)?.instance;
	}

	/**
	 * The instance to add to from outside a Live Watch (the XPeripherals view): the sidebar when no
	 * panel is open, otherwise the user's choice, with the Live Watch used last offered first.
	 */
	async pickTarget(title: string): Promise<LiveWatchInstance | undefined> {
		if (this.panels.size === 0) {
			return this.sidebar;
		}
		const items = [this.sidebar, ...Array.from(this.panels.values()).map((p) => p.instance)]
			.map((instance) => ({ label: this.nameOf(instance), instance }))
			.sort((a, b) => Number(b.instance.id === this.lastUsedId) - Number(a.instance.id === this.lastUsedId));
		const picked = await vscode.window.showQuickPick(items, { title, placeHolder: 'Live Watch to add to' });
		if (picked) {
			this.lastUsedId = picked.instance.id;
		}
		return picked?.instance;
	}

	/** Brings a Live Watch on screen, e.g. after adding to it. */
	async reveal(instance: LiveWatchInstance): Promise<void> {
		const panel = this.panels.get(instance.id);
		if (panel) {
			panel.panel.reveal(undefined, true);
		} else {
			await vscode.commands.executeCommand(`${LiveWatchManager.viewType}.focus`);
		}
	}

	/** Lists the saved panels to open, rename or delete, and offers a new one. */
	async showPanelPicker(): Promise<void> {
		type Item = vscode.QuickPickItem & { saved?: SavedPanel; create?: boolean };
		const newWindowButton = { iconPath: new vscode.ThemeIcon('empty-window'), tooltip: 'Open in New Window' };
		const renameButton = { iconPath: new vscode.ThemeIcon('edit'), tooltip: 'Rename' };
		const deleteButton = { iconPath: new vscode.ThemeIcon('trash'), tooltip: 'Delete with its expressions' };
		const qp = vscode.window.createQuickPick<Item>();
		qp.title = 'Open Live Watch Panel';
		qp.placeholder = 'Pick a saved Live Watch or create a new one';
		let busy = false;
		const update = () => {
			const items: Item[] = [{ label: '$(add) New Live Watch Panel...', create: true, alwaysShow: true }];
			if (this.saved.length > 0) {
				items.push({ label: 'Saved', kind: vscode.QuickPickItemKind.Separator });
			}
			for (const saved of this.saved) {
				const count = this.context.workspaceState.get<unknown[]>(expressionsKey(saved.id), []).length;
				items.push({
					label: saved.name,
					description: [
						`${count} expression${count === 1 ? '' : 's'}`,
						saved.intervalMs !== undefined ? `${saved.intervalMs} ms` : 'global interval',
						this.panels.has(saved.id) ? 'open' : '',
					].filter(Boolean).join(' · '),
					buttons: [newWindowButton, renameButton, deleteButton],
					saved,
				});
			}
			qp.items = items;
		};
		update();
		qp.onDidAccept(() => {
			const item = qp.selectedItems[0];
			qp.hide();
			if (item?.create) {
				void this.createPanel();
			} else if (item?.saved) {
				void this.openPanel(item.saved);
			}
		});
		qp.onDidTriggerItemButton(async (e) => {
			const saved = e.item.saved;
			if (!saved) {
				return;
			}
			if (e.button === newWindowButton) {
				qp.hide();
				await this.openPanel(saved, true);
				return;
			}
			// The input box and the modal hide the quick pick; it is shown again afterwards.
			busy = true;
			try {
				if (e.button === renameButton) {
					await this.renamePanel(saved.id);
				} else if (e.button === deleteButton) {
					await this.deletePanel(saved.id);
				}
			} finally {
				busy = false;
			}
			update();
			qp.show();
		});
		qp.onDidHide(() => {
			if (!busy) {
				qp.dispose();
			}
		});
		qp.show();
	}

	/** Asks for a name and opens a new, empty Live Watch panel. */
	async createPanel(): Promise<void> {
		let n = 2;
		while (this.saved.some((p) => p.name.toLowerCase() === `live watch ${n}`)) {
			n++;
		}
		const name = await this.askName('New Live Watch Panel', `Live Watch ${n}`);
		if (!name) {
			return;
		}
		const saved: SavedPanel = { id: crypto.randomBytes(6).toString('hex'), name };
		this.saved.push(saved);
		await this.save();
		await this.openPanel(saved);
	}

	/** Shows a saved panel, opening it if it is closed; optionally in a floating window. */
	async openPanel(saved: SavedPanel, newWindow = false): Promise<void> {
		const open = this.panels.get(saved.id);
		if (open) {
			open.panel.reveal();
		} else {
			// Beside the source, which is what a watch is usually looked at with.
			const panel = vscode.window.createWebviewPanel(LiveWatchManager.panelViewType, saved.name, vscode.ViewColumn.Beside);
			this.attachPanel(saved, panel);
		}
		if (newWindow) {
			try {
				await vscode.commands.executeCommand('workbench.action.moveEditorToNewWindow');
			} catch {
				void vscode.window.showInformationMessage('Opening a Live Watch in a new window needs VS Code 1.85 or newer; drag its tab out instead.');
			}
		}
	}

	/** Moves the active Live Watch panel into a floating window. */
	async moveActivePanelToNewWindow(): Promise<void> {
		const instance = this.activePanelInstance();
		const saved = instance && this.saved.find((p) => p.id === instance.id);
		if (saved) {
			await this.openPanel(saved, true);
		}
	}

	async renamePanel(id: string | undefined): Promise<void> {
		const saved = this.saved.find((p) => p.id === id);
		if (!saved) {
			return;
		}
		const name = await this.askName('Rename Live Watch Panel', saved.name, saved.id);
		if (!name || name === saved.name) {
			return;
		}
		saved.name = name;
		await this.save();
		this.panels.get(saved.id)?.rename(name);
	}

	async deletePanel(id: string): Promise<void> {
		const saved = this.saved.find((p) => p.id === id);
		if (!saved) {
			return;
		}
		const remove = 'Delete';
		const answer = await vscode.window.showWarningMessage(
			`Delete the Live Watch "${saved.name}" and its expressions?`, { modal: true }, remove);
		if (answer !== remove) {
			return;
		}
		this.panels.get(id)?.panel.dispose();
		this.saved = this.saved.filter((p) => p !== saved);
		await this.save();
		await this.context.workspaceState.update(expressionsKey(id), undefined);
	}

	/** Gives an instance its own refresh interval, or makes it follow the global one again. */
	async configureInterval(instance: LiveWatchInstance): Promise<void> {
		const picked = await pickIntervalMs({
			title: `Live Watch Refresh Interval: ${this.nameOf(instance)}`,
			placeHolder: 'How often this Live Watch re-reads the target while it runs',
			current: instance.intervalOverrideMs,
			globalMs: liveUpdateIntervalMs(),
		});
		if (picked === undefined) {
			return;
		}
		const intervalMs = picked === 'global' ? undefined : picked;
		instance.setIntervalOverride(intervalMs);
		if (instance === this.sidebar) {
			await this.context.workspaceState.update(SIDEBAR_INTERVAL_KEY, instance.intervalOverrideMs);
			return;
		}
		const saved = this.saved.find((p) => p.id === instance.id);
		if (saved) {
			saved.intervalMs = instance.intervalOverrideMs;
			await this.save();
		}
	}

	/** Whether some open Live Watch uses its own refresh interval, for the status bar tooltip. */
	hasIntervalOverride(): boolean {
		return this.sidebar.intervalOverrideMs !== undefined
			|| Array.from(this.panels.values()).some((p) => p.instance.intervalOverrideMs !== undefined);
	}

	private nameOf(instance: LiveWatchInstance): string {
		return instance === this.sidebar ? 'Live Watch (sidebar)' : this.saved.find((p) => p.id === instance.id)?.name ?? 'Live Watch';
	}

	private async restorePanel(panel: vscode.WebviewPanel, id: unknown): Promise<void> {
		const saved = this.saved.find((p) => p.id === id);
		if (!saved || this.panels.has(saved.id)) {
			// Deleted meanwhile, or restored twice.
			panel.dispose();
			return;
		}
		this.attachPanel(saved, panel);
	}

	private attachPanel(saved: SavedPanel, panel: vscode.WebviewPanel): void {
		const instance = new LiveWatchInstance(this.context, {
			id: saved.id,
			storageKey: expressionsKey(saved.id),
			intervalMs: saved.intervalMs,
			onDidFocus: () => (this.lastUsedId = saved.id),
		});
		const open = new LiveWatchPanel(panel, instance, saved.name, () => (this.lastUsedId = saved.id), () => {
			if (this.panels.get(saved.id) === open) {
				this.panels.delete(saved.id);
			}
		});
		this.panels.set(saved.id, open);
		this.lastUsedId = saved.id;
	}

	private askName(title: string, value: string, ownId?: string): Promise<string | undefined> {
		return Promise.resolve(vscode.window.showInputBox({
			title,
			value,
			prompt: 'Name of the Live Watch panel',
			ignoreFocusOut: true,
			validateInput: (input) => {
				const name = input.trim();
				if (name === '') {
					return 'A name is required.';
				}
				return this.saved.some((p) => p.id !== ownId && p.name.toLowerCase() === name.toLowerCase())
					? 'A Live Watch with this name exists already.'
					: undefined;
			},
		})).then((name) => name?.trim());
	}

	private async save(): Promise<void> {
		await this.context.workspaceState.update(PANELS_KEY, this.saved);
	}
}

/** The Live Watch view in the Run and Debug sidebar. */
class LiveWatchSidebarProvider implements vscode.WebviewViewProvider {
	constructor(private readonly instance: LiveWatchInstance) {}

	resolveWebviewView(view: vscode.WebviewView): void {
		const host: LiveWatchHost = {
			webview: view.webview,
			get visible() {
				return view.visible;
			},
			setDescription: (description) => (view.description = description),
		};
		this.instance.attach(host);
		view.onDidChangeVisibility(() => this.instance.setVisible(view.visible));
		view.onDidDispose(() => this.instance.detach(host));
	}
}

/** A Live Watch in an editor tab. The instance lives as long as the panel. */
class LiveWatchPanel implements LiveWatchHost {
	private description = '';

	constructor(
		readonly panel: vscode.WebviewPanel,
		readonly instance: LiveWatchInstance,
		private name: string,
		onActive: () => void,
		onClosed: () => void,
	) {
		instance.attach(this);
		// A tab in the background reads nothing; one in a floating window counts as visible.
		panel.onDidChangeViewState(() => {
			instance.setVisible(panel.visible);
			if (panel.active) {
				onActive();
			}
		});
		panel.onDidDispose(() => {
			instance.detach(this);
			instance.dispose();
			onClosed();
		});
		this.updateTitle();
	}

	get webview(): vscode.Webview {
		return this.panel.webview;
	}

	get visible(): boolean {
		return this.panel.visible;
	}

	setDescription(description: string): void {
		this.description = description;
		this.updateTitle();
	}

	rename(name: string): void {
		this.name = name;
		this.updateTitle();
	}

	/** A panel has no description, so the refresh state goes into the title, like "Motor · 500 ms". */
	private updateTitle(): void {
		this.panel.title = this.description ? `${this.name} · ${this.description}` : this.name;
	}
}

/**
 * Asks for a refresh interval: a preset, a custom value or - when `globalMs` is given - "use the
 * global interval" (returned as 'global'). `current` undefined means the global one is in use.
 */
export async function pickIntervalMs(options: {
	title: string;
	placeHolder: string;
	current: number | undefined;
	globalMs?: number;
}): Promise<number | 'global' | undefined> {
	const { current, globalMs } = options;
	type Item = vscode.QuickPickItem & { intervalMs?: number; global?: boolean; custom?: boolean };
	const items: Item[] = [];
	if (globalMs !== undefined) {
		items.push({
			label: `Use global interval (${globalMs} ms)`,
			description: current === undefined ? 'Current' : '',
			detail: 'Set in the status bar; also used by the AUTOSAR OS views',
			global: true,
		});
	}
	items.push(
		...INTERVAL_OPTIONS.map((intervalMs) => ({
			label: `${intervalMs} ms`,
			description: current === intervalMs ? 'Current' : '',
			intervalMs,
		})),
		{
			label: 'Custom...',
			description: current !== undefined && !INTERVAL_OPTIONS.includes(current) ? `Current: ${current} ms` : '',
			custom: true,
		},
	);
	const picked = await vscode.window.showQuickPick(items, {
		title: options.title,
		placeHolder: `${options.placeHolder} (minimum 200 ms)`,
		ignoreFocusOut: true,
	});
	if (!picked) {
		return undefined;
	}
	if (picked.global) {
		return 'global';
	}
	if (!picked.custom) {
		return picked.intervalMs;
	}
	const custom = await vscode.window.showInputBox({
		title: options.title,
		placeHolder: 'Refresh interval in milliseconds (minimum 200 ms)',
		value: String(current ?? globalMs ?? 1000),
		ignoreFocusOut: true,
		validateInput: (input) => {
			const trimmed = input.trim();
			if (trimmed === '') {
				return 'Interval is required.';
			}
			const parsed = Number(trimmed);
			return !Number.isFinite(parsed) || parsed < 200 ? 'Minimum 200 ms' : undefined;
		},
	});
	return custom === undefined ? undefined : Number(custom.trim());
}
