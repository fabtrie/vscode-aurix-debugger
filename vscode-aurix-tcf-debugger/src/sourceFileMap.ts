import * as fs from 'fs';
import * as path from 'path';

/** Maps the source paths in the debug information (`from`) onto the local checkout (`to`). */
export interface SourcePrefixMapping {
	from: string;
	to: string;
}

/** Keeps the mappings learned by suffix matching or "Locate File" across debug sessions. */
export interface SourceMappingStore {
	load(): SourcePrefixMapping[];
	save(mappings: SourcePrefixMapping[]): void;
}

/** Most learned mappings kept; the newest replace the oldest. */
const MAX_LEARNED = 32;
/** Fewest trailing path components a suffix match may use, so a bare file name never matches. */
const MIN_SUFFIX_COMPONENTS = 2;

function components(p: string): string[] {
	return p.replace(/\\/g, '/').split('/').filter(c => c !== '' && c !== '.');
}

/** Windows paths compare case-insensitively. */
function sameComponent(a: string, b: string): boolean {
	return a.toLowerCase() === b.toLowerCase();
}

/** The components of `p` after `prefix`, or undefined when `p` is not under `prefix`. */
function restAfter(p: string, prefix: string): string[] | undefined {
	const pc = components(p);
	const xc = components(prefix);
	if (xc.length === 0 || xc.length > pc.length) {
		return undefined;
	}
	for (let i = 0; i < xc.length; i++) {
		if (!sameComponent(pc[i], xc[i])) {
			return undefined;
		}
	}
	return pc.slice(xc.length);
}

function joinLocal(base: string, rest: string[]): string {
	return path.resolve(base, ...rest);
}

export function isAbsolutePath(p: string): boolean {
	return path.isAbsolute(p) || /^[a-zA-Z]:[\\/]/.test(p);
}

function normalizeMapping(m: SourcePrefixMapping): SourcePrefixMapping | undefined {
	if (typeof m?.from !== 'string' || typeof m?.to !== 'string' || m.from.trim() === '' || m.to.trim() === '') {
		return undefined;
	}
	return { from: m.from.trim().replace(/\\/g, '/').replace(/\/+$/, ''), to: path.resolve(m.to.trim()) };
}

/**
 * The mapping that turns `debugPath` into `localPath`: both paths without their common trailing
 * components (at least the file name). Undefined when the file names differ.
 */
export function deriveMapping(debugPath: string, localPath: string): SourcePrefixMapping | undefined {
	const dc = components(debugPath);
	const lc = components(localPath);
	let common = 0;
	while (common < dc.length - 1 && common < lc.length - 1 && sameComponent(dc[dc.length - 1 - common], lc[lc.length - 1 - common])) {
		common++;
	}
	if (common === 0) {
		return undefined;
	}
	const from = dc.slice(0, dc.length - common).join('/');
	const to = lc.slice(0, lc.length - common).join('/');
	// Keep the root of an absolute POSIX path.
	return normalizeMapping({
		from: debugPath.replace(/\\/g, '/').startsWith('/') ? `/${from}` : from,
		to: localPath.replace(/\\/g, '/').startsWith('/') ? `/${to}` : to
	});
}

/**
 * Resolves source paths from the debug information that do not exist locally, e.g. objects a
 * compiler cache built in another checkout:
 * 1. "sourceFileMap" prefixes from launch.json (applied even when the original path exists),
 * 2. learned prefixes,
 * 3. suffix matching: the path without its leading folders, under each search root. A hit is learned
 *    as a prefix mapping, so the other files of that checkout resolve directly.
 */
export class SourceFileResolver {
	private readonly explicit: SourcePrefixMapping[];
	private learned: SourcePrefixMapping[];
	private readonly searchRoots: string[];
	private readonly unresolved = new Set<string>();

	constructor(
		explicit: Record<string, string> | undefined,
		searchRoots: string[],
		private readonly store?: SourceMappingStore,
		private readonly exists: (p: string) => boolean = fs.existsSync
	) {
		this.explicit = Object.entries(explicit ?? {})
			.map(([from, to]) => normalizeMapping({ from, to }))
			.filter((m): m is SourcePrefixMapping => !!m)
			.sort((a, b) => components(b.from).length - components(a.from).length);
		let learned: SourcePrefixMapping[] = [];
		try {
			learned = (store?.load() ?? []).map(normalizeMapping).filter((m): m is SourcePrefixMapping => !!m);
		} catch {
			// Unreadable store: start without learned mappings.
		}
		this.learned = learned;
		const seen = new Set<string>();
		this.searchRoots = searchRoots
			.filter(r => typeof r === 'string' && r.trim() !== '')
			.map(r => path.resolve(r))
			.filter(r => {
				const k = r.toLowerCase();
				if (seen.has(k)) {
					return false;
				}
				seen.add(k);
				return true;
			});
	}

	get mappings(): readonly SourcePrefixMapping[] {
		return [...this.explicit, ...this.learned];
	}

	/** The local file for an absolute debug-information path, or undefined when none is found. */
	toLocal(debugPath: string): string | undefined {
		const mapped = this.applyMappings(this.explicit, debugPath);
		if (mapped) {
			return mapped;
		}
		if (this.exists(debugPath)) {
			return debugPath;
		}
		const key = debugPath.replace(/\\/g, '/').toLowerCase();
		if (this.unresolved.has(key)) {
			return undefined;
		}
		const learned = this.applyMappings(this.learned, debugPath);
		if (learned) {
			return learned;
		}
		const found = this.findBySuffix(debugPath);
		if (found) {
			const m = deriveMapping(debugPath, found);
			if (m) {
				this.learn(m);
			}
			return found;
		}
		this.unresolved.add(key);
		return undefined;
	}

	/**
	 * The path to send to the debugger for a local file under a mapped checkout: the part below the
	 * mapping root. The debugger matches files by path suffix, so this finds the file whether the
	 * object was compiled in this checkout or the other one. Undefined for files outside every mapping.
	 */
	toDebugger(localPath: string): string | undefined {
		let best: string[] | undefined;
		for (const m of this.mappings) {
			const rest = restAfter(localPath, m.to);
			if (rest && rest.length > 0 && (!best || rest.length < best.length)) {
				best = rest;
			}
		}
		return best?.join('/');
	}

	/** Adds a mapping (e.g. from "Locate File") and retries the paths that did not resolve. */
	learn(mapping: SourcePrefixMapping): void {
		const m = normalizeMapping(mapping);
		if (!m) {
			return;
		}
		const key = m.from.toLowerCase();
		this.learned = [m, ...this.learned.filter(x => x.from.toLowerCase() !== key)].slice(0, MAX_LEARNED);
		this.unresolved.clear();
		try {
			this.store?.save(this.learned);
		} catch {
			// Not persisted; still used in this session.
		}
	}

	private applyMappings(mappings: SourcePrefixMapping[], debugPath: string): string | undefined {
		for (const m of mappings) {
			const rest = restAfter(debugPath, m.from);
			if (rest) {
				const candidate = joinLocal(m.to, rest);
				if (this.exists(candidate)) {
					return candidate;
				}
			}
		}
		return undefined;
	}

	private findBySuffix(debugPath: string): string | undefined {
		if (!isAbsolutePath(debugPath)) {
			return undefined;
		}
		const pc = components(debugPath);
		// Longest suffix first: it is the most specific match.
		for (let drop = 1; pc.length - drop >= MIN_SUFFIX_COMPONENTS; drop++) {
			const tail = pc.slice(drop);
			for (const root of this.searchRoots) {
				const candidate = joinLocal(root, tail);
				if (this.exists(candidate)) {
					return candidate;
				}
			}
		}
		return undefined;
	}
}
