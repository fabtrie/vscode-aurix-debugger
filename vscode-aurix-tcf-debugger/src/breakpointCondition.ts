import { parseTaskFilter, TaskFilter } from './tcf/osaware';

/**
 * Per-core breakpoints, written like a Live Watch expression's core: `@N` plants the breakpoint
 * on core N only (the core of context `S0.D0.C<N>`), `@hsm` on the HSM, `@scr` on the standby
 * controller (SCR), `@0,2,hsm` on several cores. Without it a breakpoint is planted on every core (an instruction breakpoint on the core
 * of the disassembly it was set in).
 *
 * A function breakpoint carries it in its name (`Os_Task_10ms@1`, `main.c:42@1`); source and
 * instruction breakpoints, which have no name, in their condition (`@1`, `task == ComTask @1`).
 *
 * Kept free of the vscode API, so the debug adapter and its tests can use it.
 */

/**
 * A core by number (`@1`), or a core by name, whatever its number on the device: the HSM (`@hsm`)
 * or the TC3xx standby controller, the XC800 core of the SCR (`@scr`).
 */
export type CoreRef = number | 'hsm' | 'scr';

/** The cores written by name, in the order they follow the numbered cores. */
const NAMED_CORES: ReadonlyArray<'hsm' | 'scr'> = ['hsm', 'scr'];

/** A core as written after `@` (`1`, `hsm`, `SCR`), or undefined if it names none. */
export function parseCoreRef(text: string): CoreRef | undefined {
	const s = text.trim().toLowerCase();
	if (/^\d+$/.test(s)) {
		return Number(s);
	}
	return NAMED_CORES.find(n => n === s);
}

/** Cores in the order they are written: by number, then the HSM and the SCR; without duplicates. */
function sortCores(cores: CoreRef[]): CoreRef[] {
	const key = (c: CoreRef) => (typeof c === 'number' ? c : Number.MAX_SAFE_INTEGER - NAMED_CORES.length + NAMED_CORES.indexOf(c));
	return Array.from(new Set(cores)).sort((a, b) => key(a) - key(b));
}

/** Splits a trailing `@N`, `@hsm`, `@scr` or `@N,M,hsm,...` off `text`. */
export function splitCoreSuffix(text: string): { text: string; cores?: CoreRef[] } {
	const m = /^(.*?)\s*@\s*((?:\d+|hsm|scr)(?:\s*,\s*(?:\d+|hsm|scr))*)\s*$/i.exec(text);
	if (!m) {
		return { text: text.trim() };
	}
	const cores = m[2].split(',').map(s => parseCoreRef(s) as CoreRef);
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
	/**
	 * The part of the condition that is neither a task filter nor a core: a C expression the
	 * debug server evaluates on the halted core, which is resumed again when it is zero.
	 */
	expression?: string;
}

/**
 * Parses a breakpoint condition: a task filter (see `parseTaskFilter`) or a C expression (`x > 3`),
 * followed by an optional core suffix.
 */
export function parseBreakpointCondition(condition: string | undefined): BreakpointCondition {
	const { text, cores } = splitCoreSuffix(condition ?? '');
	if (text === '') {
		return { cores };
	}
	const task = parseTaskFilter(text);
	return task ? { task, cores } : { cores, expression: text };
}

/** How a named core reads in a sentence, e.g. `the HSM`. */
export function describeNamedCore(core: 'hsm' | 'scr'): string {
	return core === 'hsm' ? 'the HSM' : 'the SCR';
}

/** How the Breakpoints view names cores, e.g. `core 1`, `cores 0, 2`, `core 0 and the HSM` or `core 0, the HSM and the SCR`. */
export function describeCores(cores: CoreRef[]): string {
	const sorted = sortCores(cores);
	const numbers = sorted.filter((c): c is number => typeof c === 'number');
	const parts = numbers.length === 0 ? [] : [numbers.length === 1 ? `core ${numbers[0]}` : `cores ${numbers.join(', ')}`];
	for (const c of sorted) {
		if (typeof c !== 'number') {
			parts.push(describeNamedCore(c));
		}
	}
	return parts.length <= 2 ? parts.join(' and ') : `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
}

/** `text` with its first letter in upper case, for a description that starts a sentence. */
export function capitalize(text: string): string {
	return text.charAt(0).toUpperCase() + text.slice(1);
}
