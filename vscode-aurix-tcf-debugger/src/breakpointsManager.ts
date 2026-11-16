// The source breakpoints VS Code has set, by file and line. No vscode import, so the tests can
// check it directly.
import * as path from 'path';

export type BreakpointTuple = { id: number; file: string; line: number; verified: boolean };

/**
 * The key a source file is known by: one file can reach the debug adapter spelled differently.
 * Before the first stop VS Code sends the editor's path (`c:\work\main.c`); after it, the path the
 * adapter reported for the stack frame (`C:\work\build\..\main.c` resolved, drive letter as in the
 * debug information).
 */
export function sourceKey(file: string, platform: NodeJS.Platform = process.platform): string {
	if (platform === 'win32') {
		return path.win32.resolve(file).toLowerCase();
	}
	return path.posix.resolve(file);
}

export class BreakpointsManager {
	configured = false;
	private counter = 1;
	private byFile = new Map<string, Map<number, BreakpointTuple>>();
	private byId = new Map<number, BreakpointTuple>();

	constructor(private readonly keyOf: (file: string) => string = f => sourceKey(f)) {}

	setConfigured() {
		this.configured = true;
	}

	setVerified(id: number, verified: boolean) {
		const b = this.byId.get(id);
		if (b) {
			b.verified = verified;
		}
	}

	allocateId(): number {
		return this.counter++;
	}

	/** Returns whether the breakpoint was still known. */
	deleteById(id: number): boolean {
		const b = this.byId.get(id);
		if (!b) {
			return false;
		}
		this.byId.delete(id);
		this.byFile.get(this.keyOf(b.file))?.delete(b.line);
		return true;
	}

	/**
	 * Makes `lines` the breakpoints of `file`, as a SetBreakpoints request does: lines no longer
	 * listed are removed, new ones get an ID. The file is matched by its key, so a request with
	 * another spelling of the same path replaces the breakpoints instead of adding to them.
	 */
	updateFile(file: string, lines: number[]): BreakpointTuple[] {
		const key = this.keyOf(file);
		let map = this.byFile.get(key);
		if (!map) {
			map = new Map();
			this.byFile.set(key, map);
		}

		const nextLines = new Set(lines);
		// Remove deleted lines
		for (const [line, tuple] of Array.from(map.entries())) {
			if (!nextLines.has(line)) {
				map.delete(line);
				this.byId.delete(tuple.id);
			}
		}
		// Add new lines; the ones that stay keep their ID and take the latest spelling of the path.
		for (const line of lines) {
			const known = map.get(line);
			if (known) {
				known.file = file;
			} else {
				const tuple: BreakpointTuple = { id: this.allocateId(), file, line, verified: false };
				map.set(line, tuple);
				this.byId.set(tuple.id, tuple);
			}
		}
		return Array.from(map.values()).sort((a, b) => a.line - b.line);
	}

	getAll(): BreakpointTuple[] {
		return Array.from(this.byId.values());
	}

	getById(id: number): BreakpointTuple | undefined {
		return this.byId.get(id);
	}
}
