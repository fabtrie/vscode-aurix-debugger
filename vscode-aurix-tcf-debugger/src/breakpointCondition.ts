import { parseTaskFilter, TaskFilter } from './tcf/osaware';

/**
 * Per-core breakpoints, written like a Live Watch expression's core: `@N` plants the breakpoint
 * on core N only (the core of context `S0.D0.C<N>`), `@hsm` on the HSM, `@0,2,hsm` on several
 * cores. Without it a breakpoint is planted on every core (an instruction breakpoint on the core
 * of the disassembly it was set in).
 *
 * A function breakpoint carries it in its name (`Os_Task_10ms@1`, `main.c:42@1`); source and
 * instruction breakpoints, which have no name, in their condition (`@1`, `task == ComTask @1`).
 *
 * Kept free of the vscode API, so the debug adapter and its tests can use it.
 */

/** A core by number (`@1`), or the HSM by name (`@hsm`), whatever its number on the device. */
export type CoreRef = number | 'hsm';

/** Cores in the order they are written: by number, the HSM last; without duplicates. */
function sortCores(cores: CoreRef[]): CoreRef[] {
	const key = (c: CoreRef) => (c === 'hsm' ? Number.MAX_SAFE_INTEGER : c);
	return Array.from(new Set(cores)).sort((a, b) => key(a) - key(b));
}

/** Splits a trailing `@N`, `@hsm` or `@N,M,hsm,...` off `text`. */
export function splitCoreSuffix(text: string): { text: string; cores?: CoreRef[] } {
	const m = /^(.*?)\s*@\s*((?:\d+|hsm)(?:\s*,\s*(?:\d+|hsm))*)\s*$/i.exec(text);
	if (!m) {
		return { text: text.trim() };
	}
	const cores = m[2].split(',').map(s => s.trim()).map(s => (/^hsm$/i.test(s) ? 'hsm' as const : Number(s)));
	return { text: m[1].trim(), cores: sortCores(cores) };
}

/** `text` with the core suffix `@...` for `cores`, replacing any it had; none: no suffix. */
export function withCoreSuffix(text: string, cores: CoreRef[] | undefined): string {
	const rest = splitCoreSuffix(text).text;
	if (!cores || cores.length === 0) {
		return rest;
	}
	const suffix = `@${sortCores(cores).join(',')}`;
	return rest === '' ? suffix : `${rest} ${suffix}`;
}

export interface BreakpointCondition {
	task?: TaskFilter;
	cores?: CoreRef[];
	/** The part of the condition that is neither a task filter nor a core, if any. */
	unsupported?: string;
}

/**
 * Parses a breakpoint condition: an optional task filter (see `parseTaskFilter`) followed by an
 * optional core suffix. The adapter cannot evaluate other expressions; they are returned as
 * `unsupported`, so the breakpoint can say so instead of quietly halting every time.
 */
export function parseBreakpointCondition(condition: string | undefined): BreakpointCondition {
	const { text, cores } = splitCoreSuffix(condition ?? '');
	if (text === '') {
		return { cores };
	}
	const task = parseTaskFilter(text);
	return task ? { task, cores } : { cores, unsupported: text };
}

/** How the Breakpoints view names cores, e.g. `core 1`, `cores 0, 2` or `core 0 and the HSM`. */
export function describeCores(cores: CoreRef[]): string {
	const numbers = sortCores(cores).filter((c): c is number => c !== 'hsm');
	const parts = numbers.length === 0 ? [] : [numbers.length === 1 ? `core ${numbers[0]}` : `cores ${numbers.join(', ')}`];
	if (cores.includes('hsm')) {
		parts.push('the HSM');
	}
	return parts.join(' and ');
}

/** `text` with its first letter in upper case, for a description that starts a sentence. */
export function capitalize(text: string): string {
	return text.charAt(0).toUpperCase() + text.slice(1);
}
