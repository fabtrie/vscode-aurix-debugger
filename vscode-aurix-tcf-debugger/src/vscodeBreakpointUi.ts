import * as vscode from 'vscode';
import { BreakpointUi, ExternalBreakpoint, ExternalBreakpointChange } from './breakpointUi';

/**
 * Shows breakpoints of other debug server clients in VS Code's Breakpoints view (source and
 * function breakpoints; VS Code has no API to add instruction breakpoints) and reports the
 * user's changes to them.
 */
export class VsCodeBreakpointUi implements BreakpointUi {
	/** Shown breakpoints by external ID. */
	private shown = new Map<string, vscode.Breakpoint>();
	private listeners: Array<(change: ExternalBreakpointChange) => void> = [];
	private subscription: vscode.Disposable;

	constructor() {
		this.subscription = vscode.debug.onDidChangeBreakpoints(e => this.onDidChange(e));
	}

	private idOf(bp: vscode.Breakpoint): string | undefined {
		for (const [id, shown] of this.shown) {
			if (shown.id === bp.id) {
				return id;
			}
		}
		return undefined;
	}

	private onDidChange(e: vscode.BreakpointsChangeEvent) {
		for (const bp of e.removed) {
			const id = this.idOf(bp);
			if (id !== undefined) {
				this.shown.delete(id);
				this.listeners.forEach(l => l({ id, removed: true }));
			}
		}
		for (const bp of e.changed) {
			const id = this.idOf(bp);
			if (id === undefined) {
				continue;
			}
			const before = this.shown.get(id)!;
			this.shown.set(id, bp);
			if (before.enabled !== bp.enabled) {
				this.listeners.forEach(l => l({ id, removed: false, enabled: bp.enabled }));
			}
		}
	}

	show(bp: ExternalBreakpoint): void {
		const old = this.shown.get(bp.id);
		if (old) {
			this.shown.delete(bp.id);
			vscode.debug.removeBreakpoints([old]);
		}
		const created = bp.kind === 'source'
			? new vscode.SourceBreakpoint(new vscode.Location(vscode.Uri.file(bp.file), new vscode.Position(Math.max(0, bp.line - 1), 0)), bp.enabled, bp.condition)
			: new vscode.FunctionBreakpoint(bp.name, bp.enabled);
		this.shown.set(bp.id, created);
		vscode.debug.addBreakpoints([created]);
	}

	hide(ids: string[]): void {
		const bps: vscode.Breakpoint[] = [];
		for (const id of ids) {
			const bp = this.shown.get(id);
			if (bp) {
				this.shown.delete(id);
				bps.push(bp);
			}
		}
		if (bps.length > 0) {
			vscode.debug.removeBreakpoints(bps);
		}
	}

	onUserChange(listener: (change: ExternalBreakpointChange) => void): void {
		this.listeners.push(listener);
	}

	dispose(): void {
		this.hide(Array.from(this.shown.keys()));
		this.listeners = [];
		this.subscription.dispose();
	}
}
