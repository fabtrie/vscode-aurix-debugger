/*
 * The AurixReverse service: reverse debugging on the hardware. Before every action (run, step,
 * source step, register or memory write) the server saves the core's registers and RAM; `back`,
 * `continue` and `goto` write an earlier state back into the halted core, which then really runs
 * from there. The server reports the stop itself (contextResumed, then contextSuspended with the
 * reason "Step back" ...); the commands answer with a report of what was restored.
 */
import { toBuffer, ValidatingCommand } from './tcfutils';

export type ReverseOp = 'enable' | 'disable' | 'status' | 'history' | 'back' | 'continue' | 'goto';

/** The launch configuration's `reverse` attribute (`true` for the defaults). */
export interface ReverseLaunchConfig {
	enabled?: boolean;
	history?: number;
	trace?: boolean;
	regions?: Array<{ start: string | number; size: number; private?: boolean; name?: string }>;
	exclude?: Array<{ symbol: string } | { start: string | number; size: number }>;
	stackSymbols?: Array<[string, string]>;
	csaSymbols?: Array<[string, string]>;
	maxHits?: number;
	strict?: boolean;
	verify?: boolean;
}

/** The service's `enable` argument for a launch configuration's `reverse` attribute. */
export function reverseEnableArgs(cfg: ReverseLaunchConfig | true, context: string | undefined): Record<string, unknown> {
	const c: ReverseLaunchConfig = cfg === true ? {} : cfg;
	return {
		Context: context,
		History: c.history,
		Trace: c.trace,
		Regions: (c.regions ?? []).map(r => ({ Start: r.start, Size: r.size, Private: r.private, Name: r.name })),
		Exclude: (c.exclude ?? []).map(r => ('symbol' in r ? { Symbol: r.symbol } : { Start: r.start, Size: r.size })),
		StackSymbols: c.stackSymbols,
		CsaSymbols: c.csaSymbols,
		MaxHits: c.maxHits,
		Strict: c.strict,
		Verify: c.verify,
	};
}

/** Whether a launch configuration's `reverse` attribute switches the mode on. */
export function reverseWanted(cfg: unknown): cfg is ReverseLaunchConfig | true {
	if (cfg === true) {
		return true;
	}
	return !!cfg && typeof cfg === 'object' && (cfg as ReverseLaunchConfig).enabled !== false;
}

export class ReverseCommand extends ValidatingCommand<any> {
	constructor(private readonly op: ReverseOp, private readonly arg?: unknown) {
		super();
	}

	service(): string {
		return 'AurixReverse';
	}

	command(): string {
		return this.op;
	}

	arguments() {
		return undefined;
	}

	override toBuffer(token: string): Buffer {
		const args = this.arg === undefined ? [] : [JSON.stringify(this.arg)];
		return toBuffer(['C', token, this.service(), this.command(), ...args]);
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}/${this.op}`;
	}

	cast(json: any): any {
		return json && typeof json === 'object' ? json : {};
	}
}

/** One line for the Debug Console from a restore report. */
export function describeRestore(r: any): string {
	if (!r || typeof r !== 'object') {
		return 'Stepped back.';
	}
	const parts: string[] = [];
	const where = r.Function ? `${r.Function}${r.File ? ` (${String(r.File).split(/[\/]/).pop()}:${r.Line})` : ''}` : (typeof r.Pc === 'number' ? `0x${r.Pc.toString(16)}` : '');
	parts.push(`Stepped back${r.Action ? ` before "${r.Action}"` : ''} to checkpoint ${r.Checkpoint}${where ? ` at ${where}` : ''}`);
	const regs = Array.isArray(r.Registers) ? r.Registers.length : 0;
	parts.push(`${regs} register${regs === 1 ? '' : 's'}, ${r.Bytes ?? 0} bytes restored${r.ByDifference ? ` by difference${r.Why ? ` (${r.Why})` : ''}` : ''}`);
	if (r.Reexecuted) {
		parts.push(`ran again to the point (${r.Hits ?? 0} breakpoint hits${r.Counted !== undefined ? `, ${r.Counted} counted first` : ''})`);
	}
	const sfr: string[] = r.NotUndone?.Peripheral ?? [];
	if (sfr.length > 0) {
		parts.push(`not undone: ${sfr.length} peripheral write${sfr.length === 1 ? '' : 's'} (${sfr.slice(0, 4).join(', ')}${sfr.length > 4 ? ', ...' : ''})`);
	}
	const prot: Array<[string, number]> = r.NotUndone?.Protected ?? [];
	if (prot.length > 0) {
		parts.push(`not undone: ${prot.length} range${prot.length === 1 ? '' : 's'} the target refused to write (${prot.slice(0, 3).map(p => `${p[0]} (${p[1]} bytes)`).join(', ')}${prot.length > 3 ? ', ...' : ''})`);
	}
	if (r.Note) {
		parts.push(String(r.Note));
	}
	let s = parts.join('; ') + `. [${r.Ms ?? '?'} ms]`;
	if (r.Diverged) {
		s += `\nThe run again took another path (${JSON.stringify(r.Diverged)}): the core stands on a real state, but not the recorded one.`;
	}
	return s;
}
