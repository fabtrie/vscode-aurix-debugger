/*
 * The refresh loop the live views share.
 *
 * Every view that reads the target while it runs wants the same behaviour, and getting any of it
 * slightly different is what makes a debugger feel unpredictable:
 *
 * - one interval, set in one place, for every live view;
 * - **nothing is read unless the view is on screen** - a hidden panel costs the target nothing;
 * - pause means "only on halt", not "stop": a paused view still updates when a core stops, the
 *   way the Watch view does;
 * - the next refresh is timed from the *start* of the previous one, so a slow read delays the
 *   next one instead of queueing another on top of it.
 *
 * The scheduler here owns the timing only. What to read, and how to render it, stays with the
 * view - which is also why the profile panel can hang the lifetime of its sampler off
 * `onActiveChanged` instead of polling at all.
 */

// Type-only: the scheduler is plain timing logic with no VS Code runtime dependency, which is
// also what lets it be tested outside the editor.
import type * as vscode from 'vscode';

/** Shortest gap between two refreshes, so a slow target is never polled back to back. */
const MIN_REFRESH_GAP_MS = 50;
/** Cores of a multi-core target stop together and each reports its own stop; coalesce them. */
const BREAK_DEBOUNCE_MS = 100;

export interface LiveUpdateOptions {
	/** One refresh. It must not throw; the view reports its own errors. */
	refresh: (onBreak: boolean) => Promise<void>;
	/**
	 * A further condition beyond "session running and view visible", e.g. the Live Watch having
	 * something to watch. Checked on every reschedule.
	 */
	canRefresh?: () => boolean;
	/** The schedule changed (interval, pause, visibility, session): update a description. */
	onChanged?: () => void;
	/**
	 * The loop started or stopped. A view that holds a resource while updating - the profiler's
	 * sampling session - starts and stops it here, so hiding the panel really does stop the
	 * reads rather than only stopping the drawing.
	 */
	onActiveChanged?: (active: boolean) => void;
}

export class LiveUpdater implements vscode.Disposable {
	private timer: NodeJS.Timeout | undefined;
	private breakTimer: NodeJS.Timeout | undefined;
	private running = false;
	private wasActive = false;
	private visible = false;
	private sessionActive = false;
	private pausedFlag = false;
	private intervalMsValue = defaultIntervalMs;

	constructor(private readonly opts: LiveUpdateOptions) {
		updaters.add(this);
		this.intervalMsValue = sharedIntervalMs;
		this.sessionActive = sharedSessionActive;
	}

	dispose(): void {
		updaters.delete(this);
		this.clearTimers();
		if (this.wasActive) {
			this.wasActive = false;
			this.opts.onActiveChanged?.(false);
		}
	}

	get intervalMs(): number {
		return this.intervalMsValue;
	}

	get paused(): boolean {
		return this.pausedFlag;
	}

	/** Whether the periodic loop is running, i.e. whether the target is being read. */
	get active(): boolean {
		return this.sessionActive && this.visible && !this.pausedFlag && (this.opts.canRefresh?.() ?? true);
	}

	/** A one-line state for a view title, matching what the Live Watch shows. */
	get description(): string {
		if (!this.sessionActive) {
			return '';
		}
		return this.pausedFlag ? 'paused' : `${this.intervalMsValue} ms`;
	}

	setVisible(visible: boolean): void {
		if (this.visible === visible) {
			return;
		}
		this.visible = visible;
		// Becoming visible refreshes at once rather than waiting out an interval.
		this.reschedule(visible ? 0 : undefined);
	}

	setSessionActive(active: boolean): void {
		this.sessionActive = active;
		this.reschedule(0);
	}

	setIntervalMs(ms: number): void {
		this.intervalMsValue = normalizeIntervalMs(ms);
		this.reschedule(0);
	}

	/** Pause means "update only on halt": the periodic loop stops, breaks still refresh. */
	setPaused(paused: boolean): void {
		if (this.pausedFlag === paused) {
			return;
		}
		this.pausedFlag = paused;
		this.reschedule(0);
	}

	togglePaused(): void {
		this.setPaused(!this.pausedFlag);
	}

	/** Re-checks `canRefresh`, for a view whose content changed. */
	kick(): void {
		this.reschedule(0);
	}

	/**
	 * A core stopped.
	 *
	 * This runs even when paused, and even when the periodic loop is off - a halt is the moment
	 * the values matter most, and reading once costs nothing while the target is stopped. A
	 * hidden view still does not read: it refreshes when it comes back.
	 */
	onBreak(): void {
		if (!this.sessionActive || !this.visible) {
			return;
		}
		if (this.breakTimer) {
			clearTimeout(this.breakTimer);
		}
		this.breakTimer = setTimeout(() => {
			this.breakTimer = undefined;
			void this.opts.refresh(true);
		}, BREAK_DEBOUNCE_MS);
	}

	/** Refreshes now, off schedule, whatever the pause state - for an explicit user action. */
	async now(): Promise<void> {
		if (!this.sessionActive || !this.visible) {
			return;
		}
		await this.opts.refresh(true);
	}

	private clearTimers(): void {
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
		if (this.breakTimer) {
			clearTimeout(this.breakTimer);
			this.breakTimer = undefined;
		}
	}

	private reschedule(delayMs: number | undefined): void {
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
		const active = this.active;
		if (active !== this.wasActive) {
			this.wasActive = active;
			this.opts.onActiveChanged?.(active);
		}
		this.opts.onChanged?.();
		if (!active || delayMs === undefined) {
			return;
		}
		this.timer = setTimeout(() => {
			this.timer = undefined;
			void this.run();
		}, delayMs);
	}

	/**
	 * One refresh, then the next timed from this one's start, so updates come at a steady rate
	 * however long a read takes.
	 */
	private async run(): Promise<void> {
		if (this.running) {
			return;
		}
		this.running = true;
		const started = Date.now();
		try {
			await this.opts.refresh(false);
		} finally {
			this.running = false;
		}
		if (!this.timer) {
			this.reschedule(Math.max(MIN_REFRESH_GAP_MS, this.intervalMsValue - (Date.now() - started)));
		}
	}
}

// --- The one interval every live view uses ---------------------------------------------------

const updaters = new Set<LiveUpdater>();
const defaultIntervalMs = 1000;
let sharedIntervalMs = defaultIntervalMs;
let sharedSessionActive = false;

/** The interval the Live Watch has always used: at least 200 ms, at most 10 s. */
export function normalizeIntervalMs(value: unknown): number {
	const numeric = typeof value === 'number' ? value : Number(value);
	if (!Number.isFinite(numeric)) {
		return defaultIntervalMs;
	}
	return Math.min(10_000, Math.max(200, Math.round(numeric)));
}

export function liveUpdateIntervalMs(): number {
	return sharedIntervalMs;
}

/** Applies one interval to every live view, so there is a single rate to reason about. */
export function setLiveUpdateIntervalMs(ms: number): void {
	sharedIntervalMs = normalizeIntervalMs(ms);
	for (const u of updaters) {
		u.setIntervalMs(sharedIntervalMs);
	}
}

/** Starts the live views when a session begins and stops them when the last one ends. */
export function setLiveUpdateSessionActive(active: boolean): void {
	sharedSessionActive = active;
	for (const u of updaters) {
		u.setSessionActive(active);
	}
}

/** A core stopped: every visible live view updates once, paused or not. */
export function notifyLiveUpdateBreak(): void {
	for (const u of updaters) {
		u.onBreak();
	}
}
