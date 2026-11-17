/*
 * The "OS Profile" panel: what ran on each core, over time.
 *
 * Sampled, not traced. The agent reads `RUNNINGTASK` every period and reports the changes; a
 * switch shorter than the period is missed. The panel says so, because a timeline that looks like
 * a trace and is not would be the most misleading thing here.
 *
 * The agent accumulates the transitions and the occupancy, so this polls a few times a second and
 * gets only what is new - the sampling rate and the redraw rate are independent.
 */

import * as vscode from 'vscode';
import type { PanelPlacement } from './debugLayout';
import { LiveUpdater } from './liveUpdate';
import { describeOsError } from './tcf/osaware';
import type { OsLane, OsLaneSetting, OsProfile, OsTransition } from './tcf/osaware';

/** One lane's blocks, as the webview draws them. */
type Lane = {
	lane: number;
	core: number;
	label: string;
	key: string;
	/** "off", "trace" or a period in µs, as set for the lane. */
	mode: 'off' | 'trace' | number;
	/** "trace", "host" or "off": how it is recorded now. */
	source: string;
	reason?: string;
	blocks: Array<{ task: string; t: number; us: number }>;
};

type View = {
	/// The agent's sampler is running.
	running: boolean;
	/// Updating only on halt - which for this panel means not sampling at all.
	paused: boolean;
	/// How often the panel asks for new transitions, from the shared live-update rate.
	pollMs: number;
	periodUs: number;
	/** Window shown, in microseconds. */
	windowUs: number;
	/** Right-hand edge of the window. */
	nowUs: number;
	lanes: Lane[];
	/** Share of the window per task, per lane, most first. */
	shares: Array<{ lane: number; label: string; task: string; us: number; percent: number }>;
	/** Non-idle share per lane. */
	load: Array<{ lane: number; label: string; percent: number }>;
	/** Data trace losses (traced lanes). */
	traceLost: number;
	/** Colour index per task, assigned in first-seen order and then kept. */
	colors: Array<{ task: string; index: number }>;
	samples: number;
	dropped: number;
	missed: number;
	message?: string;
};

/** Task names that mean "nothing is running", which are never counted as load. */
const IDLE = /^(INVALID_TASK|IdleTask|.*_?Idle(Task)?(_.*)?)$/i;

export class OsProfilePanel {
	static readonly viewType = 'aurixOsProfile';
	private static current: OsProfilePanel | undefined;
	/** Rate per lane (`core:attr`), kept while VS Code runs so reopening the panel keeps it. */
	private static readonly laneModes = new Map<string, 'off' | 'trace' | number>();

	private readonly disposables: vscode.Disposable[] = [];
	private readonly live: LiveUpdater;
	private busy = false;
	private running = false;
	private periodUs = 1000;
	private windowUs = 200_000;
	/** Every transition seen, per lane, oldest first. Trimmed to the window plus a margin. */
	private readonly perCore = new Map<number, OsTransition[]>();
	/** The lanes the server reports (all of them, recorded or off). */
	private lanes: OsLane[] = [];
	private traceLost = 0;
	private lastT = 0;
	private nowUs = 0;
	private samples = 0;
	private dropped = 0;
	private missed = 0;
	/** Colour slot per task, in first-seen order: a filter must never repaint the survivors. */
	private readonly colorOf = new Map<string, number>();
	private message: string | undefined;

	private constructor(
		private readonly panel: vscode.WebviewPanel,
		private readonly mediaRoot: vscode.Uri,
	) {
		panel.webview.options = { enableScripts: true, localResourceRoots: [mediaRoot] };
		panel.webview.html = this.html(panel.webview);
		this.live = new LiveUpdater({
			refresh: () => this.poll(),
			// Sampling costs the target a bus access every period, so it is tied to the loop: a
			// hidden or paused panel stops the sampler outright rather than just not drawing.
			onActiveChanged: active => void (active ? this.startSampling() : this.stopSampling()),
			onChanged: () => this.post(),
			// Starting the sampler clears its statistics, so a halt must not stop it.
			pauseWhileHalted: false,
		});
		this.live.setVisible(panel.visible);
		this.disposables.push(
			this.live,
			panel.webview.onDidReceiveMessage(m => void this.onMessage(m)),
			panel.onDidChangeViewState(() => this.live.setVisible(panel.visible)),
			vscode.debug.onDidTerminateDebugSession(() => void this.stopSampling()),
		);
		panel.onDidDispose(() => this.dispose());
	}


	static show(extensionUri: vscode.Uri, placement?: PanelPlacement): void {
		const media = vscode.Uri.joinPath(extensionUri, 'media');
		if (OsProfilePanel.current) {
			OsProfilePanel.current.panel.reveal(placement?.viewColumn, placement?.preserveFocus);
			return;
		}
		const column = placement?.viewColumn ?? vscode.ViewColumn.Active;
		const panel = vscode.window.createWebviewPanel(OsProfilePanel.viewType, 'OS Profile', { viewColumn: column, preserveFocus: placement?.preserveFocus }, {
			enableScripts: true,
			retainContextWhenHidden: true,
			localResourceRoots: [media],
		});
		OsProfilePanel.current = new OsProfilePanel(panel, media);
	}

	dispose(): void {
		// Sampling costs the target a bus access per period, so it must not outlive the panel.
		void this.stopSampling();
		vscode.Disposable.from(...this.disposables).dispose();
		if (OsProfilePanel.current === this) {
			OsProfilePanel.current = undefined;
		}
	}

	private session(): vscode.DebugSession | undefined {
		const s = vscode.debug.activeDebugSession;
		return s?.type === 'aurixdbg' ? s : undefined;
	}

	private async onMessage(m: any): Promise<void> {
		if (m?.type === "period") {
			this.periodUs = Number(m.periodUs) || this.periodUs;
			await this.restartSampling();
		} else if (m?.type === 'laneMode' && typeof m.key === 'string') {
			const mode = m.mode === 'off' || m.mode === 'trace' ? m.mode : Number(m.mode);
			if (mode === 'trace') {
				// One data trace: tracing this lane takes it from any other.
				for (const [k, v] of OsProfilePanel.laneModes) {
					if (v === 'trace' && k !== m.key) {
						OsProfilePanel.laneModes.delete(k);
					}
				}
			}
			OsProfilePanel.laneModes.set(m.key, typeof mode === 'number' && !(mode > 0) ? this.periodUs : mode);
			await this.restartSampling();
		} else if (m?.type === "pause") {
			// Pause here stops the sampler outright, not just the drawing.
			this.live.togglePaused();
		} else if (m?.type === 'window') {
			this.windowUs = Number(m.windowUs) || this.windowUs;
			this.post();
		} else if (m?.type === 'clear') {
			this.perCore.clear();
			this.colorOf.clear();
			this.samples = 0;
			this.post();
		}
	}

	/** Starts the agent's sampler. Called by the scheduler when the panel becomes live. */
	private async startSampling(): Promise<void> {
		const session = this.session();
		if (!session || this.running) {
			return;
		}
		try {
			const lanes: OsLaneSetting[] = [...OsProfilePanel.laneModes].map(([k, Mode]) => {
				const [core, Attr] = k.split(':');
				return { Core: Number(core), Attr, Mode };
			});
			const r = (await session.customRequest('aurix/osProfileStart', { periodUs: this.periodUs, lanes })) as { PeriodUs: number; Cores: number; Lanes?: OsLane[] };
			this.periodUs = r.PeriodUs;
			this.lanes = r.Lanes ?? [];
			this.running = true;
			this.message = undefined;
			this.perCore.clear();
			this.lastT = 0;
		} catch (e) {
			this.running = false;
			this.message = describeOsError(e);
		}
		this.post();
	}

	/** Stops the agent's sampler, so a hidden or paused panel costs the target nothing. */
	private async stopSampling(): Promise<void> {
		if (!this.running) {
			return;
		}
		this.running = false;
		try {
			await this.session()?.customRequest('aurix/osProfileStop');
		} catch {
			// The session may already be gone, which is the same outcome.
		}
		this.post();
	}

	/** Restarts the sampler, which is what changing the period amounts to. */
	private async restartSampling(): Promise<void> {
		await this.stopSampling();
		if (this.live.active) {
			await this.startSampling();
		}
	}

	private async poll(): Promise<void> {
		const session = this.session();
		if (!session || this.busy || !this.running) {
			return;
		}
		this.busy = true;
		try {
			const p = (await session.customRequest('aurix/osProfile', { sinceUs: this.lastT })) as OsProfile;
			if (!p.Running) {
				// The agent dropped the session (a reset, another client): reflect it rather
				// than polling a profiler that is not there.
				this.running = false;
				this.post();
				return;
			}
			this.nowUs = p.NowUs ?? this.nowUs;
			this.periodUs = p.PeriodUs ?? this.periodUs;
			this.dropped = p.Dropped ?? 0;
			this.missed = p.Missed ?? 0;
			this.traceLost = p.TraceLost ?? 0;
			if (p.Lanes) {
				this.lanes = p.Lanes;
			}
			this.message = p.Error;
			for (const t of p.Transitions ?? []) {
				this.lastT = Math.max(this.lastT, t.T);
				this.samples += 1;
				if (!this.colorOf.has(t.Task) && !IDLE.test(t.Task)) {
					this.colorOf.set(t.Task, this.colorOf.size);
				}
				const key = t.Lane ?? t.Core;
				const lane = this.perCore.get(key) ?? [];
				lane.push(t);
				this.perCore.set(key, lane);
			}
			this.trim();
			this.post();
		} catch (e) {
			this.message = describeOsError(e);
			this.post();
		} finally {
			this.busy = false;
		}
	}

	/** Drops transitions older than the window, keeping the one that covers its left edge. */
	private trim(): void {
		const cut = this.nowUs - this.windowUs;
		for (const [core, lane] of this.perCore) {
			// Keep the last transition at or before the cut: it is what the window opens inside.
			let keepFrom = 0;
			for (let i = 0; i < lane.length; i++) {
				if (lane[i].T <= cut) {
					keepFrom = i;
				} else {
					break;
				}
			}
			if (keepFrom > 0) {
				this.perCore.set(core, lane.slice(keepFrom));
			}
		}
	}

	/** Turns the transitions into blocks clipped to the window, and the shares they imply. */
	private view(): View {
		const from = Math.max(0, this.nowUs - this.windowUs);
		const lanes: Lane[] = [];
		const shares: View['shares'] = [];
		const load: View['load'] = [];

		// Every lane the server reports (off ones too, so they can be switched on); an older server
		// reports none: one lane per core then.
		const known: OsLane[] = this.lanes.length > 0
			? this.lanes
			: [...this.perCore.keys()].map((core) => ({ Lane: core, Core: core, Attr: 'RUNNINGTASK', Mode: this.periodUs, PeriodUs: this.periodUs, Source: 'host' }));
		for (const info of [...known].sort((a, b) => a.Core - b.Core || a.Lane - b.Lane)) {
			const core = info.Lane;
			const lane = this.perCore.get(core) ?? [];
			const blocks: Lane['blocks'] = [];
			const label = `cpu${info.Core}` + (info.Attr === 'RUNNINGTASK' ? '' : ` ${info.Attr.replace(/^RUNNING/, '').toLowerCase()}`);
			for (let i = 0; i < lane.length; i++) {
				const start = Math.max(lane[i].T, from);
				const end = i + 1 < lane.length ? lane[i + 1].T : this.nowUs;
				if (end <= from) {
					continue;
				}
				blocks.push({ task: lane[i].Task, t: start - from, us: Math.max(0, end - start) });
			}
			lanes.push({
				lane: info.Lane, core: info.Core, label, key: `${info.Core}:${info.Attr}`,
				mode: info.Mode, source: info.Source, reason: info.Reason ?? undefined, blocks,
			});
			if (info.Source === 'off') {
				continue;
			}

			const total = blocks.reduce((s, b) => s + b.us, 0);
			const byTask = new Map<string, number>();
			for (const b of blocks) {
				byTask.set(b.task, (byTask.get(b.task) ?? 0) + b.us);
			}
			let busy = 0;
			for (const [task, us] of byTask) {
				shares.push({ lane: info.Lane, label, task, us, percent: total > 0 ? (us * 100) / total : 0 });
				if (!IDLE.test(task)) {
					busy += us;
				}
			}
			load.push({ lane: info.Lane, label, percent: total > 0 ? (busy * 100) / total : 0 });
		}
		shares.sort((a, b) => a.lane - b.lane || b.us - a.us);

		return {
			running: this.running,
			paused: this.live.paused,
			pollMs: this.live.intervalMs,
			periodUs: this.periodUs,
			windowUs: this.windowUs,
			nowUs: this.nowUs,
			lanes,
			shares,
			load,
			colors: [...this.colorOf].map(([task, index]) => ({ task, index })),
			samples: this.samples,
			dropped: this.dropped,
			missed: this.missed,
			traceLost: this.traceLost,
			message: this.message,
		};
	}

	private post(): void {
		void this.panel.webview.postMessage({ type: 'profile', view: this.view() });
	}

	private html(webview: vscode.Webview): string {
		const css = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'osProfile.css'));
		const js = webview.asWebviewUri(vscode.Uri.joinPath(this.mediaRoot, 'osProfile.js'));
		const nonce = Math.random().toString(36).slice(2) + Date.now().toString(36);
		return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
<link rel="stylesheet" href="${css}">
<title>OS Profile</title>
</head>
<body>
<div id="toolbar">
  <button id="run" type="button">Pause</button>
  <label for="period" class="lbl" title="Rate of the lanes without one of their own">Default rate</label>
  <select id="period">
    <option value="100">100 µs</option>
    <option value="250">250 µs</option>
    <option value="1000" selected>1 ms</option>
    <option value="5000">5 ms</option>
  </select>
  <label class="sr" for="window">Window</label>
  <select id="window">
    <option value="200000" selected>200 ms</option>
    <option value="1000000">1 s</option>
    <option value="10000000">10 s</option>
  </select>
  <span id="stats"></span>
  <span class="spacer"></span>
  <span class="note" id="kind">sampled, not traced</span>
  <button id="clear" type="button">Clear</button>
</div>
<div id="message"></div>
<h2 class="h">Running task per core <span class="sub">rate per lane: off, a period, or trace (every write, CPU0)</span></h2>
<div id="lanes"></div>
<div id="axis"></div>
<div id="legend"></div>
<div id="bottom">
  <div id="sharesBox">
    <h2 class="h">Share of the window</h2>
    <div id="shares"></div>
  </div>
  <div id="loadBox">
    <h2 class="h">CPU load <span class="sub">non-idle share</span></h2>
    <div id="load"></div>
    <p class="note wrap">A sampled lane reads its OS variable at its rate and reports only the changes: a switch shorter than the rate is not seen at all, so it is a statistical profile. A traced lane records every write with the miniMCDS data trace and the target's time stamp: exact, for one lane of CPU0 at a time. Hatched: the trace lost data.</p>
  </div>
</div>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}
}
