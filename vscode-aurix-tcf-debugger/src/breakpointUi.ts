/**
 * Breakpoints of other clients of the debug server (e.g. set through aurix-dbg's REST/MCP API),
 * shown in VS Code's Breakpoints view.
 *
 * The debug adapter decides what to show; the extension implements this interface with the
 * vscode API (the adapter itself does not depend on it, so it also runs outside VS Code).
 */

/**
 * A breakpoint of another client, as VS Code shows it: at a source line when its address is the
 * start of one, else as a function breakpoint named by symbol or address.
 */
export type ExternalBreakpoint =
	| { id: string; enabled: boolean; kind: 'source'; file: string; line: number; condition?: string }
	| { id: string; enabled: boolean; kind: 'function'; name: string };

/** What the user did with a shown breakpoint in VS Code. */
export type ExternalBreakpointChange = { id: string; removed: true } | { id: string; removed: false; enabled: boolean };

export interface BreakpointUi {
	/** Shows a breakpoint, or updates a shown one with the same `id`. */
	show(bp: ExternalBreakpoint): void;
	/** Removes shown breakpoints (without reporting it as a user change). */
	hide(ids: string[]): void;
	/** Called when the user removes, enables or disables a shown breakpoint. */
	onUserChange(listener: (change: ExternalBreakpointChange) => void): void;
	/** Removes all shown breakpoints and stops listening. */
	dispose(): void;
}
