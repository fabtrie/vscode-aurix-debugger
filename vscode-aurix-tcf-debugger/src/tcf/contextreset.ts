import { parseEmptyResponse, SimpleCommand, toBuffer, ValidatingCommand } from './tcfutils';

export interface ContextResetCapability {
	Type: string;
	Description?: string;
}

function asContextResetCapability(v: any): ContextResetCapability | null {
	if (!v || typeof v !== 'object') {
		return null;
	}
	const type = (v as any).Type;
	if (typeof type !== 'string' || type.trim() === '') {
		return null;
	}
	const desc = (v as any).Description;
	return {
		Type: type,
		Description: typeof desc === 'string' ? desc : undefined,
	};
}

function asContextResetCapabilities(json: any): ContextResetCapability[] {
	if (!Array.isArray(json)) {
		return [];
	}
	const out: ContextResetCapability[] = [];
	for (const v of json) {
		const cap = asContextResetCapability(v);
		if (cap) {
			out.push(cap);
		}
	}
	return out;
}

abstract class ContextResetCommand<T> extends SimpleCommand<T> {
	service(): string {
		return 'ContextReset';
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}`;
	}
}

abstract class ContextResetValidatingCommand<T> extends ValidatingCommand<T> {
	service(): string {
		return 'ContextReset';
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}`;
	}
}

export class GetCapabilitiesContextResetCommand extends ContextResetValidatingCommand<ContextResetCapability[]> {
	constructor() {
		super();
	}

	command(): string {
		return 'getCapabilities';
	}

	arguments() {
		return undefined;
	}

	cast(json: any): ContextResetCapability[] {
		return asContextResetCapabilities(json);
	}
}

export class ResetContextResetCommand extends ContextResetCommand<undefined> {
	constructor(
		private readonly contextId: string,
		private readonly type: string,
		private readonly suspendAfterReset: boolean,
	) {
		super();
	}

	command(): string {
		return 'reset';
	}

	arguments() {
		return undefined;
	}

	toBuffer(token: string): Buffer {
		// ContextReset.reset expects: <context-id> <type> <params>
		// ADS/Eclipse sends `{}` for the params in the common "run after reset" case.
		// To match that behavior (and avoid agent quirks), we only include Suspend when true.
		const params = this.suspendAfterReset ? { Suspend: true } : {};
		return toBuffer(
			['C', token, this.service(), this.command(), JSON.stringify(this.contextId), JSON.stringify(this.type)],
			params,
		);
	}

	result(responseAll: Buffer[], success: (v: undefined) => void, error: any): void {
		parseEmptyResponse(responseAll, success, error);
	}
}
