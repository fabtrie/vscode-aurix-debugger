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
import { LiveUpdater } from './liveUpdate';
import { describeOsError } from './tcf/osaware';
import type { OsProfile, OsTransition } from './tcf/osaware';

/** One lane's blocks, as the webview draws them. */
type Lane = {
	core: number;
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
	/** Share of the window per task, per core, most first. */
	shares: Array<{ core: number; task: string; us: number; percent: number }>;
	/** Non-idle share per core. */
	load: Array<{ core: number; percent: number }>;
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
	private static current: OsProfilePanel | undefined;

	private readonly disposables: vscode.Disposable[] = [];
	private readonly live: LiveUpdater;
	private busy = false;
	private running = false;
	private periodUs = 1000;
	private windowUs = 200_000;
	/** Every transition seen, per core, oldest first. Trimmed to the window plus a margin. */
	private readonly perCore = new Map<number, OsTransition[]>();
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


	static show(extensionUri: vscode.Uri): void {
		const media = vscode.Uri.joinPath(extensionUri, 'media');
		if (OsProfilePanel.current) {
			OsProfilePanel.current.panel.reveal();
			return;
		}
		const panel = vscode.window.createWebviewPanel('aurixOsProfile', 'OS Profile', vscode.ViewColumn.Active, {
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
			const r = (await session.customRequest('aurix/osProfileStart', { periodUs: this.periodUs })) as { PeriodUs: number; Cores: number };
			this.periodUs = r.PeriodUs;
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
			this.message = p.Error;
			for (const t of p.Transitions ?? []) {
				this.lastT = Math.max(this.lastT, t.T);
				this.samples += 1;
				if (!this.colorOf.has(t.Task) && !IDLE.test(t.Task)) {
					this.colorOf.set(t.Task, this.colorOf.size);
				}
				const lane = this.perCore.get(t.Core) ?? [];
				lane.push(t);
				this.perCore.set(t.Core, lane);
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

		for (const core of [...this.perCore.keys()].sort((a, b) => a - b)) {
			const lane = this.perCore.get(core) ?? [];
			const blocks: Lane['blocks'] = [];
			for (let i = 0; i < lane.length; i++) {
				const start = Math.max(lane[i].T, from);
				const end = i + 1 < lane.length ? lane[i + 1].T : this.nowUs;
				if (end <= from) {
					continue;
				}
				blocks.push({ task: lane[i].Task, t: start - from, us: Math.max(0, end - start) });
			}
			lanes.push({ core, blocks });

			const total = blocks.reduce((s, b) => s + b.us, 0);
			const byTask = new Map<string, number>();
			for (const b of blocks) {
				byTask.set(b.task, (byTask.get(b.task) ?? 0) + b.us);
			}
			let busy = 0;
			for (const [task, us] of byTask) {
				shares.push({ core, task, us, percent: total > 0 ? (us * 100) / total : 0 });
				if (!IDLE.test(task)) {
					busy += us;
				}
			}
			load.push({ core, percent: total > 0 ? (busy * 100) / total : 0 });
		}
		shares.sort((a, b) => a.core - b.core || b.us - a.us);

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
  <label class="sr" for="period">Sampling period</label>
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
  <span class="note">sampled, not traced</span>
  <button id="clear" type="button">Clear</button>
</div>
<div id="message"></div>
<h2 class="h">Running task per core</h2>
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
    <p class="note wrap">The sampler reads <code>RUNNINGTASK</code> for every core in one coalesced bus access and reports only the changes. A switch shorter than the sampling period is not seen at all, so this is a statistical profile — not an MCDS trace.</p>
  </div>
</div>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
	}
}
