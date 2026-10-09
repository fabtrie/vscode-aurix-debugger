/*******************************************************************************
 * Debug layout: remembers where the debug tabs were and gives them back to the next session.
 *
 * When the last AURIX session ends, the debug tabs (Disassembly, Live Watch panels, the OS panels,
 * memory views opened as editors) are recorded together with the editor grid and closed, so
 * editing code is not cluttered with them. The next session restores the grid and reopens them
 * where they were. The Disassembly view opens only while the target is stopped, so its place is
 * handed to the code that opens it on the first stop.
 *******************************************************************************/

import * as vscode from 'vscode';
import { LiveWatchManager } from './liveWatchManager';
import { OsObjectsPanel } from './osObjectsPanel';
import { OsProfilePanel } from './osProfilePanel';

/** Where a panel opens: its editor group, and whether the focus stays where it is. */
export interface PanelPlacement {
	viewColumn: vscode.ViewColumn;
	preserveFocus?: boolean;
}

type DebugTabKind = 'disassembly' | 'liveWatch' | 'osObjects' | 'osProfile' | 'memory';

interface SavedTab {
	kind: DebugTabKind;
	column: number;
	/** The tab shown in its group. */
	active: boolean;
	/** Live Watch panel id, or the memory editor's URI. */
	key?: string;
}

interface SavedLayout {
	/** The result of vscode.getEditorLayout: group orientation, nesting and sizes. */
	editorLayout?: unknown;
	/** In group order, then tab order. */
	tabs: SavedTab[];
	/** Set when the tabs were closed and the next session should reopen them. */
	pending: boolean;
}

const LAYOUT_KEY = 'aurix-debugger.debugLayout';
/** Memory Inspector's custom editor: its URI holds the memory reference, so it can be reopened. */
const MEMORY_EDITOR_VIEW_TYPE = 'memory-inspector.inspect';
/** VS Code's Disassembly view has no input type in the tab API; its label is all there is. */
const DISASSEMBLY_LABEL = 'Disassembly';
/** As long as the agent shutdown delay, so a Restart (terminate + start) keeps the tabs. */
const CLOSE_DELAY_MS = 1000;
const GROUP_ORDINALS = ['First', 'Second', 'Third', 'Fourth', 'Fifth', 'Sixth', 'Seventh', 'Eighth'];

/** Focuses the editor group of a view column (1-8), so the next editor opens there. */
export async function focusEditorGroup(column: number): Promise<boolean> {
	const ordinal = GROUP_ORDINALS[column - 1];
	if (!ordinal) {
		return false;
	}
	try {
		await vscode.commands.executeCommand(`workbench.action.focus${ordinal}EditorGroup`);
		return true;
	} catch {
		return false;
	}
}

export class DebugLayout {
	private closeTimer: NodeJS.Timeout | undefined;
	/** Counts session starts, so an end whose snapshot is overtaken by a Restart closes nothing. */
	private startCount = 0;
	/** Where the next Disassembly view opens, from the restored layout. */
	private disassemblyColumn: number | undefined;

	constructor(
		private readonly context: vscode.ExtensionContext,
		private readonly liveWatches: LiveWatchManager | undefined,
		private readonly log: (line: string) => void,
	) {}

	/** The last AURIX session ended: record the debug tabs now, close them unless a session follows. */
	async sessionsEnded(): Promise<void> {
		this.disassemblyColumn = undefined;
		if (!this.enabled()) {
			return;
		}
		const starts = this.startCount;
		const snapshot = await this.snapshot();
		if (snapshot.tabs.length === 0 || starts !== this.startCount) {
			// Nothing to give back: keep the previous layout for the next session.
			return;
		}
		this.cancelClose();
		this.closeTimer = setTimeout(() => {
			this.closeTimer = undefined;
			void this.stash(snapshot);
		}, CLOSE_DELAY_MS);
	}

	/** An AURIX session started: reopen the stashed tabs, unless they are still open (Restart). */
	async sessionStarted(): Promise<void> {
		this.startCount++;
		if (this.closeTimer) {
			this.cancelClose();
			return;
		}
		const saved = this.saved();
		if (!this.enabled() || !saved?.pending) {
			return;
		}
		await this.context.workspaceState.update(LAYOUT_KEY, { ...saved, pending: false });
		if (this.mainWindowTabs(this.groupCount(await this.currentEditorLayout())).length > 0) {
			// Debug tabs survived (closed by hand meanwhile or so): leave the layout alone.
			return;
		}
		await this.restore(saved);
	}

	/** The column of the Disassembly view of the restored layout, once. */
	takeDisassemblyColumn(): number | undefined {
		const column = this.disassemblyColumn;
		this.disassemblyColumn = undefined;
		return column;
	}

	/** Forgets the remembered layout: the next session opens the default one. */
	async reset(): Promise<void> {
		this.cancelClose();
		this.disassemblyColumn = undefined;
		await this.context.workspaceState.update(LAYOUT_KEY, undefined);
	}

	private enabled(): boolean {
		return vscode.workspace.getConfiguration('aurix-debugger').get<boolean>('rememberDebugLayout', true);
	}

	private saved(): SavedLayout | undefined {
		const saved = this.context.workspaceState.get<SavedLayout>(LAYOUT_KEY);
		return saved && Array.isArray(saved.tabs) ? saved : undefined;
	}

	private cancelClose(): void {
		if (this.closeTimer) {
			clearTimeout(this.closeTimer);
			this.closeTimer = undefined;
		}
	}

	private async snapshot(): Promise<SavedLayout> {
		const editorLayout = await this.currentEditorLayout();
		const tabs = this.mainWindowTabs(this.groupCount(editorLayout)).map((f) => f.saved);
		return { editorLayout, tabs, pending: false };
	}

	private async stash(snapshot: SavedLayout): Promise<void> {
		await this.context.workspaceState.update(LAYOUT_KEY, { ...snapshot, pending: true });
		const found = this.mainWindowTabs(this.groupCount(await this.currentEditorLayout()));
		const groups = new Set(found.map(({ tab }) => tab.group));
		try {
			await vscode.window.tabGroups.close(found.map(({ tab }) => tab), true);
			// A group left empty goes too, also a locked one (the Disassembly's).
			const empty = vscode.window.tabGroups.all.filter((g) => groups.has(g) && g.tabs.length === 0);
			if (empty.length > 0 && vscode.window.tabGroups.all.length > empty.length) {
				await vscode.window.tabGroups.close(empty, true);
			}
		} catch (e) {
			this.log(`Failed to close the debug tabs: ${String(e)}`);
		}
		this.log(`Debug layout saved: ${snapshot.tabs.map((t) => `${t.kind}@${t.column}`).join(', ')}.`);
	}

	private async restore(saved: SavedLayout): Promise<void> {
		if (saved.editorLayout) {
			try {
				await vscode.commands.executeCommand('vscode.setEditorLayout', saved.editorLayout);
			} catch (e) {
				this.log(`Failed to restore the editor layout: ${String(e)}`);
			}
		}
		const opened: SavedTab[] = [];
		for (const tab of saved.tabs) {
			if (tab.kind === 'disassembly') {
				this.disassemblyColumn = tab.column;
			} else if (await this.open(tab)) {
				opened.push(tab);
			}
		}
		// Opening a tab shows it; bring each group's former tab to the front again.
		for (const tab of opened) {
			if (tab.active && opened.filter((t) => t.column === tab.column).length > 1) {
				await this.open(tab);
			}
		}
		this.log(`Debug layout restored: ${saved.tabs.map((t) => `${t.kind}@${t.column}`).join(', ')}.`);
	}

	private async open(tab: SavedTab): Promise<boolean> {
		const placement: PanelPlacement = { viewColumn: tab.column, preserveFocus: true };
		const extensionUri = this.context.extensionUri;
		try {
			switch (tab.kind) {
				case 'liveWatch':
					return !!tab.key && !!this.liveWatches?.openPanelById(tab.key, placement);
				case 'osObjects':
					OsObjectsPanel.show(extensionUri, undefined, placement);
					return true;
				case 'osProfile':
					OsProfilePanel.show(extensionUri, placement);
					return true;
				case 'memory':
					if (!tab.key) {
						return false;
					}
					await vscode.commands.executeCommand('vscode.openWith', vscode.Uri.parse(tab.key), MEMORY_EDITOR_VIEW_TYPE, placement);
					return true;
				default:
					return false;
			}
		} catch (e) {
			this.log(`Failed to reopen ${tab.kind}: ${String(e)}`);
			return false;
		}
	}

	private async currentEditorLayout(): Promise<unknown> {
		try {
			return await vscode.commands.executeCommand('vscode.getEditorLayout');
		} catch {
			return undefined;
		}
	}

	/** Groups of the main window, from the layout; groups after them are in floating windows. */
	private groupCount(layout: unknown): number {
		const leaves = (node: unknown): number => {
			const groups = (node as { groups?: unknown[] } | undefined)?.groups;
			return Array.isArray(groups) && groups.length > 0 ? groups.reduce<number>((n, g) => n + leaves(g), 0) : 1;
		};
		return layout ? leaves(layout) : vscode.window.tabGroups.all.length;
	}

	/** The debug tabs of the main window, in group and tab order. */
	private mainWindowTabs(groupCount: number): { tab: vscode.Tab; saved: SavedTab }[] {
		const found: { tab: vscode.Tab; saved: SavedTab }[] = [];
		for (const group of vscode.window.tabGroups.all) {
			if (group.viewColumn > groupCount) {
				continue;
			}
			for (const tab of group.tabs) {
				const kind = this.classify(tab);
				if (kind) {
					found.push({ tab, saved: { ...kind, column: group.viewColumn, active: tab.isActive } });
				}
			}
		}
		return found;
	}

	private classify(tab: vscode.Tab): { kind: DebugTabKind; key?: string } | undefined {
		const input = tab.input;
		if (input === undefined && tab.label === DISASSEMBLY_LABEL) {
			return { kind: 'disassembly' };
		}
		if (input instanceof vscode.TabInputWebview) {
			// Extension webviews carry a prefix in front of the view type they were created with.
			const is = (viewType: string) => input.viewType === viewType || input.viewType.endsWith(`-${viewType}`);
			if (is(LiveWatchManager.panelViewType)) {
				const key = this.liveWatches?.panelIdByTitle(tab.label);
				return key ? { kind: 'liveWatch', key } : undefined;
			}
			if (is(OsObjectsPanel.viewType)) {
				return { kind: 'osObjects' };
			}
			if (is(OsProfilePanel.viewType)) {
				return { kind: 'osProfile' };
			}
			// Memory Inspector's own panels ("Memory N") do not tell their address: left open.
			return undefined;
		}
		if (input instanceof vscode.TabInputCustom && input.viewType === MEMORY_EDITOR_VIEW_TYPE) {
			return { kind: 'memory', key: input.uri.toString() };
		}
		return undefined;
	}
}
