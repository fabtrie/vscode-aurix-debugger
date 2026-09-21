/*
 * The "AUTOSAR OS Objects" panel: one tab per ORTI object type, each a table of objects.
 *
 * A webview rather than a tree, because these are tables - 38 tasks by a dozen columns does not
 * fit a sidebar. The columns are not hardcoded: they are the attributes the ORTI file declares
 * for the type, in the order it declares them, with the generator's own descriptions as headers.
 *
 * Refreshing asks only for the attributes that can change (`liveOnly`), and the constant ones are
 * merged from the first full read. On the file this was built against that is 613 single-address
 * reads against 657 attributes that never need one.
 */

import * as vscode from 'vscode';
import { LiveUpdater } from './liveUpdate';
import { describeOsError } from './tcf/osaware';
import type { OsInfo, OsObjectList, OsStack, OsStackList, OsTypeInfo } from './tcf/osaware';

/** One table as the webview renders it. */
type Table = {
	kind: string;
	description: string;
	columns: Array<{ name: string; header: string; perCore: boolean }>;
	rows: Array<{ name: string; cells: Array<{ text: string; link?: string; error?: string; muted: boolean; pct?: number }> }>;
	reads: number;
	tabs: Array<{ kind: string; count: number; active: boolean }>;
	/// The type supports a measured column set (stack coverage), so the panel offers the action.
	measurable?: boolean;
	/// Updating only on halt.
	paused?: boolean;
	intervalMs?: number;
	error?: string;
};

/** Values that mean "nothing here". */
const NOTHING = new Set(['INVALID_TASK', 'INVALID_ISR', '-', '']);

export class OsObjectsPanel {
	private static current: OsObjectsPanel | undefined;

	private readonly disposables: vscode.Disposable[] = [];
	private kind = 'TASK';
	private info: OsInfo | undefined;
	/** The last full read per type, so a live-only refresh keeps the constant columns. */
	private readonly full = new Map<string, OsObjectList>();
	private readonly live: LiveUpdater;
	/// The last table posted, so a pause or an interval change can redraw the header alone.
	private lastTable: Table | undefined;
	private busy = false;
	/// Measured stack coverage, by stack name. Only ever filled on request.
	private coverage = new Map<string, OsStack>();
	private coverageReads = 0;

	private constructor(
		private readonly panel: vscode.WebviewPanel,
		private readonly mediaRoot: vscode.Uri,
	) {
		panel.webview.options = { enableScripts: true, localResourceRoots: [mediaRoot] };
		panel.webview.html = this.html(panel.webview);
		this.live = new LiveUpdater({
			refresh: () => this.load(false),
			onChanged: () => this.post(this.lastTable ?? this.empty()),
		});
		this.live.setVisible(panel.visible);
		this.disposables.push(
			this.live,
			panel.webview.onDidReceiveMessage(m => void this.onMessage(m)),
			// A hidden tab reads nothing: the tables are the most expensive thing here.
			panel.onDidChangeViewState(() => this.live.setVisible(panel.visible)),
			vscode.debug.onDidTerminateDebugSession(() => void this.load(true)),
		);
		panel.onDidDispose(() => this.dispose());
		void this.load(true);
	}


	static show(extensionUri: vscode.Uri, kind?: string): void {
		const media = vscode.Uri.joinPath(extensionUri, 'media');
		if (OsObjectsPanel.current) {
			OsObjectsPanel.current.panel.reveal(vscode.ViewColumn.Active);
			if (kind) {
				void OsObjectsPanel.current.select(kind);
			}
			return;
		}
		const panel = vscode.window.createWebviewPanel('aurixOsObjects', 'AUTOSAR OS', vscode.ViewColumn.Active, {
			enableScripts: true,
			retainContextWhenHidden: true,
			localResourceRoots: [media],
		});
		OsObjectsPanel.current = new OsObjectsPanel(panel, media);
		if (kind) {
			void OsObjectsPanel.current.select(kind);
		}
	}

	dispose(): void {
		vscode.Disposable.from(...this.disposables).dispose();
		if (OsObjectsPanel.current === this) {
			OsObjectsPanel.current = undefined;
		}
	}

	private session(): vscode.DebugSession | undefined {
		const s = vscode.debug.activeDebugSession;
		return s?.type === 'aurixdbg' ? s : undefined;
	}

	private async onMessage(m: any): Promise<void> {
		if (m?.type === 'select' && typeof m.kind === 'string') {
			await this.select(m.kind);
		} else if (m?.type === 'refresh') {
			await this.load(true);
		} else if (m?.type === 'measure') {
			await this.measureStacks();
		} else if (m?.type === 'pause') {
			this.live.togglePaused();
		} else if (m?.type === 'follow' && typeof m.link === 'string') {
			// An enumerator that names another object: jump to the type that has it.
			const target = this.info?.Types?.find(t => t.Name !== this.kind && this.kindHasObject(t, String(m.link)));
			if (target) {
				await this.select(target.Name, String(m.link));
			}
		}
	}

	private kindHasObject(t: OsTypeInfo, name: string): boolean {
		return this.full.get(t.Name)?.Objects.some(o => o.Name === name) ?? false;
	}

	private async select(kind: string, highlight?: string): Promise<void> {
		if (kind !== this.kind) {
			// Coverage belongs to the type it was measured for.
			this.coverage.clear();
			this.coverageReads = 0;
		}
		this.kind = kind;
		await this.load(true, highlight);
	}

	/**
	 * Reads the current type.
	 *
	 * `force` does a full read (every attribute); otherwise only the ones that can change, merged
	 * over the last full read. That keeps a 1 Hz refresh to the reads it actually needs.
	 */
	private async load(force: boolean, highlight?: string): Promise<void> {
		const session = this.session();
		if (this.busy) {
			return;
		}
		this.busy = true;
		try {
			if (!session) {
				this.post({ ...this.empty(), error: 'No AURIX debug session is running.' }, highlight);
				return;
			}
			if (!this.info?.Loaded || force) {
				this.info = (await session.customRequest('aurix/osInfo')) as OsInfo;
			}
			if (!this.info?.Loaded) {
				this.post({ ...this.empty(), error: 'No ORTI file loaded. Set "ortiFile" in the launch configuration, or use “Load ORTI File…”.' }, highlight);
				return;
			}
			if (!this.info.Types?.some(t => t.Name === this.kind)) {
				this.kind = this.info.Types?.find(t => t.Count > 0)?.Name ?? this.info.Types?.[0]?.Name ?? this.kind;
			}
			const wantFull = force || !this.full.has(this.kind);
			const list = (await session.customRequest('aurix/osObjects', { kind: this.kind, liveOnly: !wantFull })) as OsObjectList;
			if (wantFull) {
				this.full.set(this.kind, list);
			} else {
				this.merge(this.kind, list);
			}
			this.post(this.table(list.Reads), highlight);
		} catch (e) {
			this.post({ ...this.empty(), error: describeOsError(e) }, highlight);
		} finally {
			this.busy = false;
		}
	}

	/**
	 * Merges measured stack coverage into the table as three extra columns and a bar.
	 *
	 * Kept out of the refresh path on purpose: finding a high-water mark means reading the
	 * untouched part of every stack, so it happens when the user asks and the result is then held
	 * until they ask again.
	 */
	private async measureStacks(): Promise<void> {
		const session = this.session();
		if (!session || this.kind !== 'STACK') {
			return;
		}
		try {
			const cov = (await session.customRequest('aurix/osStacks')) as OsStackList;
			this.coverage = new Map(cov.Stacks.map(s => [s.Name, s]));
			this.coverageReads = cov.Reads;
			this.post(this.table(0));
		} catch (e) {
			this.post({ ...this.empty(), error: describeOsError(e) });
		}
	}

	/** Folds a live-only read into the last full one, so the constant columns stay filled. */
	private merge(kind: string, live: OsObjectList): void {
		const base = this.full.get(kind);
		if (!base) {
			this.full.set(kind, live);
			return;
		}
		const byName = new Map(base.Objects.map(o => [o.Name, o]));
		for (const o of live.Objects) {
			const target = byName.get(o.Name);
			if (!target) {
				continue;
			}
			for (const a of o.Attributes) {
				const i = target.Attributes.findIndex(x => x.Name === a.Name && x.Core === a.Core);
				if (i >= 0) {
					target.Attributes[i] = a;
				} else {
					target.Attributes.push(a);
				}
			}
		}
	}

	private tabs(): Table['tabs'] {
		return (this.info?.Types ?? []).map(t => ({ kind: t.Name, count: t.Count, active: t.Name === this.kind }));
	}

	private empty(): Table {
		return { kind: this.kind, description: '', columns: [], rows: [], reads: 0, tabs: this.tabs() };
	}

	private table(reads: number): Table {
		const type = this.info?.Types?.find(t => t.Name === this.kind);
		const list = this.full.get(this.kind);
		if (!type || !list) {
			return this.empty();
		}
		// One column per declared attribute; a per-core attribute gets one column per core.
		const cores = this.info?.Cores ?? 1;
		const columns: Table['columns'] = [];
		for (const a of type.Attributes) {
			if (a.PerCore) {
				for (let c = 0; c < cores; c++) {
					columns.push({ name: `${a.Name}@${c}`, header: `${a.Description || a.Name} [${c}]`, perCore: true });
				}
			} else {
				columns.push({ name: a.Name, header: a.Description || a.Name, perCore: false });
			}
		}
		const rows: Table['rows'] = list.Objects.map(o => {
			const by = new Map(o.Attributes.map(a => [a.Core === undefined ? a.Name : `${a.Name}@${a.Core}`, a]));
			return {
				name: o.Name,
				cells: columns.map(col => {
					const a = by.get(col.name);
					if (!a) {
						return { text: '', muted: true };
					}
					if (a.Error) {
						return { text: '—', error: a.Error, muted: true };
					}
					const nothing = NOTHING.has(a.Value);
					return { text: nothing ? '—' : a.Value, link: a.Link, muted: nothing };
				}),
			};
		});

		// Stack coverage is not an ORTI attribute: it is measured, and only when asked for.
		const measurable = this.kind === 'STACK';
		if (measurable && this.coverage.size > 0) {
			columns.push(
				{ name: '@used', header: 'Used', perCore: false },
				{ name: '@free', header: 'Free', perCore: false },
				{ name: '@hwm', header: 'High-water mark', perCore: false },
			);
			for (const r of rows) {
				const s = this.coverage.get(r.name);
				if (!s || s.Used === undefined) {
					const why = s?.Error ?? 'not measured';
					r.cells.push({ text: '—', muted: true }, { text: '—', muted: true }, { text: why, error: s?.Error, muted: true });
					continue;
				}
				r.cells.push(
					{ text: String(s.Used), muted: false },
					{ text: String(s.Free ?? s.Size - s.Used), muted: false },
					{ text: `${s.Percent ?? 0}%`, muted: false, pct: s.Percent ?? 0 },
				);
			}
		}
		return {
			kind: this.kind,
			description: type.Description,
			columns,
			rows,
			reads: reads + this.coverageReads,
			tabs: this.tabs(),
			measurable,
		};
	}

	private post(t: Table, highlight?: string): void {
		this.lastTable = t;
		const table: Table = { ...t, paused: this.live.paused, intervalMs: this.live.intervalMs };
		void this.panel.webview.postMessage({ type: "table", table, highlight });
	}

	/** Pause means "only on halt", as in the Live Watch. */
	togglePaused(): void {
		this.live.togglePaused();
	}

	private html(webview: vscode.Webview): string {
		const css = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'osObjects.css'));
		const js = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'osObjects.js'));
		const nonce = Math.random().toString(36).slice(2) + Date.now().toString(36);
		return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
<link rel="stylesheet" href="${css}">
<title>AUTOSAR OS</title>
</head>
<body>
<div id="tabs" role="tablist"></div>
<div id="toolbar">
  <label class="sr" for="filter">Filter objects</label>
  <input id="filter" type="text" placeholder="Filter by name or value" autocomplete="off">
  <span id="count"></span>
  <span class="spacer"></span>
  <span id="reads"></span>
  <button id="pause" type="button">Pause</button>
  <button id="measure" type="button" hidden>Measure coverage</button>
  <button id="refresh" type="button">Refresh</button>
</div>
<div id="message"></div>
<div id="grid" tabindex="0"></div>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}
}
