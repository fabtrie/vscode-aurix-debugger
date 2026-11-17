/*
 * The AurixTrace service: arm a miniMCDS trace, check a configuration, read the status, stop and
 * read the trace RAM, and get the recording decoded (program flow with function and source line,
 * or data trace writes), and the instruction trace: pages of instructions with the registers
 * rebuilt from the halt (`insns`, `regs`, `find`) and the replay of a trace position in the
 * debugger (`replay`, `replayMove`). Every command answers with free-form JSON.
 */
import { toBuffer, ValidatingCommand } from './tcfutils';

export type TraceOp = 'arm' | 'check' | 'status' | 'stop' | 'read' | 'insns' | 'regs' | 'find' | 'replay' | 'replayMove';

/** Trace configuration as the service takes it. */
export interface TraceConfig {
	Core?: number;
	Kind?: 'program' | 'data';
	Ranges?: Array<{ Function: string } | { Start: string | number; Len: number }>;
	Stop?: 'halt' | 'full' | 'manual' | { Function: string };
	Timestamps?: 'tsu' | 'ticks';
}

export class TraceCommand extends ValidatingCommand<any> {
	constructor(private readonly op: TraceOp, private readonly arg?: unknown) {
		super();
	}

	service(): string {
		return 'AurixTrace';
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
