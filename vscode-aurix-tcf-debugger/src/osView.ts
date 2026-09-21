/*
 * The "AUTOSAR OS" view in the Run and Debug sidebar.
 *
 * A tree rather than a webview: it is read-only, so the native tree gives the right look, the
 * twisties and the keyboard handling for free. The object tables, which need columns, are a
 * separate webview panel.
 *
 * What it shows comes from the ORTI file, not from anything hardcoded here. The per-core rows are
 * the ORTI 2.3 SMP extension: `RUNNINGTASK[0]`, `RUNNINGTASK[1]` and friends.
 */

import * as vscode from 'vscode';
import { LiveUpdater } from './liveUpdate';
import { describeOsError } from './tcf/osaware';
import type { OsAttributeValue, OsInfo, OsObject, OsObjectList } from './tcf/osaware';

/** Attributes of the OS object worth putting in a narrow sidebar, in this order. */
const CORE_ROWS: Array<{ attr: string; label: string }> = [
	{ attr: 'RUNNINGTASK', label: 'Running task' },
	{ attr: 'RUNNINGTASKPRIORITY', label: 'Priority' },
	{ attr: 'RUNNINGISR2', label: 'Running ISR2' },
	{ attr: 'SERVICETRACE', label: 'Last service' },
	{ attr: 'LASTERROR', label: 'Last error' },
	{ attr: 'CURRENTAPPMODE', label: 'App mode' },
	{ attr: 'vs_COREID', label: 'Core ID' },
];

/** Values that mean "nothing here", which should read as a dash rather than as a name. */
const NOTHING = new Set(['INVALID_TASK', 'INVALID_ISR', '0', '-']);

export type Node =
	| { kind: 'message'; text: string; icon?: string }
	| { kind: 'os'; info: OsInfo }
	| { kind: 'core'; core: number; label: string }
	| { kind: 'value'; label: string; value: string; error?: string }
	| { kind: 'types' }
	| { kind: 'type'; name: string; description: string; count: number };

export class OsViewProvider implements vscode.TreeDataProvider<Node>, vscode.Disposable {
	static readonly viewId = 'aurixAutosarOs';

	private readonly changed = new vscode.EventEmitter<Node | undefined>();
	readonly onDidChangeTreeData = this.changed.event;

	private info: OsInfo | undefined;
	/** The OS object's attribute values, keyed `NAME` or `NAME@core`. */
	private values = new Map<string, OsAttributeValue>();
	private error: string | undefined;
	private refreshing = false;
	private view: vscode.TreeView<Node> | undefined;
	private readonly live: LiveUpdater;
	private readonly disposables: vscode.Disposable[] = [];

	constructor() {
		this.live = new LiveUpdater({
			refresh: () => this.refresh(),
			onChanged: () => this.updateDescription(),
		});
		this.disposables.push(
			this.live,
			vscode.debug.onDidChangeActiveDebugSession(() => this.onSessionChanged()),
			vscode.debug.onDidTerminateDebugSession(() => this.onSessionChanged()),
		);
	}

	/**
	 * Binds the tree view.
	 *
	 * A `TreeDataProvider` on its own cannot tell whether anyone is looking, so the view is
	 * created rather than only registered: a collapsed or hidden AUTOSAR OS section must not read
	 * the target at all.
	 */
	attach(view: vscode.TreeView<Node>): void {
		this.view = view;
		this.live.setVisible(view.visible);
		this.disposables.push(view.onDidChangeVisibility(e => this.live.setVisible(e.visible)));
		this.updateDescription();
	}

	dispose(): void {
		vscode.Disposable.from(...this.disposables).dispose();
		this.changed.dispose();
	}

	/** Pause means "only on halt", as in the Live Watch. */
	togglePaused(): void {
		this.live.togglePaused();
	}

	private updateDescription(): void {
		if (this.view) {
			this.view.description = this.live.description;
		}
		void vscode.commands.executeCommand('setContext', 'aurixOs.paused', this.live.paused);
	}

	private session(): vscode.DebugSession | undefined {
		const s = vscode.debug.activeDebugSession;
		return s?.type === 'aurixdbg' ? s : undefined;
	}

	private onSessionChanged(): void {
		this.info = undefined;
		this.values.clear();
		this.error = undefined;
		this.changed.fire(undefined);
		this.live.kick();
	}

	/** Re-reads the OS object. One refresh is a handful of bus accesses, so it can be periodic. */
	async refresh(): Promise<void> {
		const session = this.session();
		if (!session || this.refreshing) {
			return;
		}
		this.refreshing = true;
		try {
			if (!this.info?.Loaded) {
				this.info = (await session.customRequest('aurix/osInfo')) as OsInfo;
			}
			if (this.info?.Loaded) {
				const list = (await session.customRequest('aurix/osObjects', { kind: 'OS' })) as OsObjectList;
				const os: OsObject | undefined = list.Objects[0];
				this.values.clear();
				for (const a of os?.Attributes ?? []) {
					this.values.set(a.Core === undefined ? a.Name : `${a.Name}@${a.Core}`, a);
				}
			}
			this.error = undefined;
		} catch (e) {
			this.error = describeOsError(e);
		} finally {
			this.refreshing = false;
			this.changed.fire(undefined);
		}
	}

	/** Loads (or reloads) an ORTI file and shows what came of it. */
	async load(file: string): Promise<void> {
		const session = this.session();
		if (!session) {
			throw new Error('no AURIX debug session is running');
		}
		this.info = (await session.customRequest('aurix/osLoad', { file })) as OsInfo;
		await this.refresh();
	}

	getTreeItem(n: Node): vscode.TreeItem {
		switch (n.kind) {
			case 'message': {
				const i = new vscode.TreeItem(n.text, vscode.TreeItemCollapsibleState.None);
				i.iconPath = n.icon ? new vscode.ThemeIcon(n.icon) : undefined;
				return i;
			}
			case 'os': {
				const i = new vscode.TreeItem(n.info.Implementation ?? 'OS', vscode.TreeItemCollapsibleState.Expanded);
				const version = n.info.Version ? `ORTI ${n.info.Version}` : 'ORTI';
				i.description = `${version} · ${n.info.Cores ?? 1} core${(n.info.Cores ?? 1) === 1 ? '' : 's'}`;
				i.tooltip = new vscode.MarkdownString(
					[
						`**${n.info.Implementation ?? 'OS'}**`,
						'',
						`- File: \`${n.info.File ?? '?'}\``,
						`- Semantics: ORTI ${n.info.Version ?? '?'}${n.info.Version === '2.3' ? ' (unofficial SMP extension)' : ''}`,
						`- Resolved against: ${n.info.Target ?? 'the first core with symbols'}`,
						n.info.Errors?.length ? `- ${n.info.Errors.length} attribute(s) not resolved yet` : '',
					]
						.filter(Boolean)
						.join('\n'),
				);
				i.contextValue = 'aurixOs.root';
				return i;
			}
			case 'core': {
				const i = new vscode.TreeItem(n.label, vscode.TreeItemCollapsibleState.Expanded);
				const task = this.valueOf('RUNNINGTASK', n.core);
				i.description = task && !NOTHING.has(task) ? task : undefined;
				i.iconPath = new vscode.ThemeIcon('chip');
				return i;
			}
			case 'value': {
				const i = new vscode.TreeItem(n.label, vscode.TreeItemCollapsibleState.None);
				i.description = n.error ? 'unavailable' : n.value;
				i.tooltip = n.error ?? n.value;
				if (n.error) {
					i.iconPath = new vscode.ThemeIcon('warning');
				}
				return i;
			}
			case 'types': {
				const i = new vscode.TreeItem('Objects', vscode.TreeItemCollapsibleState.Collapsed);
				const total = (this.info?.Types ?? []).reduce((s, t) => s + t.Count, 0);
				i.description = `${total}`;
				return i;
			}
			case 'type': {
				const i = new vscode.TreeItem(n.name, vscode.TreeItemCollapsibleState.None);
				i.description = `${n.count}`;
				i.tooltip = n.description || n.name;
				// Empty types stay listed: the file declared them, and saying so is more useful
				// than pretending they are not there.
				i.command = n.count > 0 ? { command: 'aurix-debugger.osShowObjects', title: 'Show', arguments: [n.name] } : undefined;
				return i;
			}
		}
	}

	private valueOf(attr: string, core: number): string | undefined {
		return (this.values.get(`${attr}@${core}`) ?? this.values.get(attr))?.Value;
	}

	getChildren(n?: Node): Node[] {
		if (!n) {
			if (!this.session()) {
				return [{ kind: 'message', text: 'No debug session', icon: 'debug-disconnect' }];
			}
			if (this.error) {
				return [{ kind: 'message', text: this.error, icon: 'warning' }];
			}
			if (!this.info) {
				return [{ kind: 'message', text: 'Reading…', icon: 'loading~spin' }];
			}
			if (!this.info.Loaded) {
				return [{ kind: 'message', text: 'No ORTI file loaded', icon: 'circle-outline' }];
			}
			return [{ kind: 'os', info: this.info }];
		}
		if (n.kind === 'os') {
			const cores = n.info.Cores ?? 1;
			const out: Node[] = [];
			for (let c = 0; c < cores; c++) {
				// The AUTOSAR core id is the useful label; the index is only a position in the file.
				const id = this.valueOf('vs_COREID', c);
				out.push({ kind: 'core', core: c, label: id && id !== String(c) ? `Core ${c} (${id})` : `Core ${c}` });
			}
			out.push({ kind: 'types' });
			return out;
		}
		if (n.kind === 'core') {
			const out: Node[] = [];
			for (const row of CORE_ROWS) {
				const v = this.values.get(`${row.attr}@${n.core}`) ?? (n.core === 0 ? this.values.get(row.attr) : undefined);
				if (!v) {
					continue;
				}
				const shown = v.Value && !NOTHING.has(v.Value) ? v.Value : '—';
				out.push({ kind: 'value', label: row.label, value: shown, error: v.Error });
			}
			return out;
		}
		if (n.kind === 'types') {
			return (this.info?.Types ?? []).map(t => ({ kind: 'type', name: t.Name, description: t.Description, count: t.Count }));
		}
		return [];
	}
}
