import { PromiseError, PromiseSuccess, SimpleCommand, parseResponse } from './tcfutils';

/**
 * aurix-dbg custom service: health of the debug probe and the target (Vref, reset line, debug
 * access), pushed by the server as events. Older servers do not announce it.
 */
export const AURIX_TARGET_SERVICE = 'AurixTarget';

export const TARGET_HEALTH_STATES = ['ok', 'unpowered', 'reset', 'noDebugAccess', 'noDevice', 'probeLost'] as const;
export type TargetHealthState = typeof TARGET_HEALTH_STATES[number];

export interface TargetHealth {
	state: TargetHealthState;
	text: string;
	vrefMv?: number;
}

/**
 * The health JSON of `getHealth` and `healthChanged`, or null for none (null JSON or an unknown
 * shape: a newer server's unknown state is shown as nothing rather than as an error).
 */
export function asTargetHealth(json: unknown): TargetHealth | null {
	if (json === null || typeof json !== 'object' || Array.isArray(json)) {
		return null;
	}
	const o = json as Record<string, unknown>;
	if (typeof o.state !== 'string' || !(TARGET_HEALTH_STATES as readonly string[]).includes(o.state)) {
		return null;
	}
	const health: TargetHealth = {
		state: o.state as TargetHealthState,
		text: typeof o.text === 'string' ? o.text : '',
	};
	if (typeof o.vrefMv === 'number' && Number.isFinite(o.vrefMv)) {
		health.vrefMv = o.vrefMv;
	}
	return health;
}

/** The single JSON argument of an AurixTarget event (a trailing empty field is allowed). */
function eventArg(datas: Buffer[]): unknown {
	const first = datas.find(d => d.length > 0);
	if (!first) {
		return null;
	}
	return JSON.parse(first.toString());
}

/** `AurixTarget.healthChanged` argument. Throws on malformed JSON. */
export function parseHealthChangedEvent(datas: Buffer[]): TargetHealth | null {
	return asTargetHealth(eventArg(datas));
}

/** `AurixTarget.probeLost` argument: why the probe is gone. Throws on malformed JSON. */
export function parseProbeLostEvent(datas: Buffer[]): string {
	const why = eventArg(datas);
	if (typeof why === 'string') {
		return why;
	}
	return why === null || why === undefined ? '' : JSON.stringify(why);
}

export class GetHealthAurixTargetCommand extends SimpleCommand<TargetHealth | null> {
	service(): string {
		return AURIX_TARGET_SERVICE;
	}

	command(): string {
		return 'getHealth';
	}

	arguments() {
		return undefined;
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}/getHealth`;
	}

	result(responseAll: Buffer[], success: PromiseSuccess<TargetHealth | null>, error: PromiseError<TargetHealth | null>): void {
		parseResponse(responseAll, success, error, asTargetHealth);
	}
}
