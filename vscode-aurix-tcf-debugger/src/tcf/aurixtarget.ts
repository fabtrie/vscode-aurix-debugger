import { JSONValidationError, PromiseError, PromiseSuccess, SimpleCommand, parseEmptyResponse, parseResponse, toBuffer } from './tcfutils';

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

// --- Live set: the values a Live Watch view reads periodically ---------------------------------
//
// A view sends its regions and its interval once (`setLiveSet`), then reads all of them with one
// `getLiveValues` per refresh. The server decides per region how to get the value (sampled by the
// probe, or a normal memory read); the client never asks which.

/** One memory region of a live set: `size` bytes at `addr`, read through context `ctx`. */
export interface LiveRegion {
	ctx: string;
	addr: number;
	size: number;
}

/** One entry of `getLiveValues`, in the order of the regions of the set. */
export interface LiveValue {
	/** The bytes, base64. */
	data?: string;
	/** How the server got the value ("daq", "read"). Diagnostics only: never branch on it. */
	source?: string;
	/** Time of the value in µs, when the server knows it. */
	tUs?: number;
	/** Why the region could not be read; set instead of `data`. */
	error?: string;
}

/**
 * The reply of `getLiveValues`. An entry's `error` may also come as a TCF error object; it is
 * reduced to its message.
 */
export function asLiveValues(json: unknown): LiveValue[] {
	if (!Array.isArray(json)) {
		throw new JSONValidationError('Not an array', json);
	}
	return json.map((item): LiveValue => {
		if (item === null || typeof item !== 'object' || Array.isArray(item)) {
			throw new JSONValidationError('Not an object', json);
		}
		const o = item as Record<string, unknown>;
		const value: LiveValue = {};
		if (typeof o.data === 'string') {
			value.data = o.data;
		}
		if (typeof o.source === 'string') {
			value.source = o.source;
		}
		if (typeof o.tUs === 'number' && Number.isFinite(o.tUs)) {
			value.tUs = o.tUs;
		}
		if (typeof o.error === 'string') {
			value.error = o.error;
		} else if (o.error !== undefined && o.error !== null) {
			const format = (o.error as Record<string, unknown>)?.Format;
			value.error = typeof format === 'string' ? format : JSON.stringify(o.error);
		}
		return value;
	});
}

/** A command with several plain JSON arguments (TCF puts each in its own field). */
abstract class AurixTargetCommand<T> extends SimpleCommand<T> {
	service(): string {
		return AURIX_TARGET_SERVICE;
	}

	arguments() {
		return undefined;
	}

	protected abstract args(): unknown[];

	toBuffer(token: string): Buffer {
		return toBuffer(['C', token, this.service(), this.command(), ...this.args().map((a) => JSON.stringify(a))], undefined);
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}/${this.command()}`;
	}
}

/** Replaces the live set `key` (a Live Watch view) with these regions, sampled every `periodUs`. */
export class SetLiveSetAurixTargetCommand extends AurixTargetCommand<undefined> {
	constructor(readonly key: string, readonly regions: LiveRegion[], readonly periodUs: number) {
		super();
	}

	command(): string {
		return 'setLiveSet';
	}

	protected args(): unknown[] {
		return [this.key, this.regions, this.periodUs];
	}

	result(responseAll: Buffer[], success: PromiseSuccess<undefined>, error: PromiseError<undefined>): void {
		parseEmptyResponse(responseAll, success, error);
	}
}

/** The current value of every region of the live set `key`, in the order they were set. */
export class GetLiveValuesAurixTargetCommand extends AurixTargetCommand<LiveValue[]> {
	constructor(readonly key: string) {
		super();
	}

	command(): string {
		return 'getLiveValues';
	}

	protected args(): unknown[] {
		return [this.key];
	}

	result(responseAll: Buffer[], success: PromiseSuccess<LiveValue[]>, error: PromiseError<LiveValue[]>): void {
		parseResponse(responseAll, success, error, asLiveValues);
	}
}

/** Drops the live set `key`: the server stops sampling for it. */
export class ClearLiveSetAurixTargetCommand extends AurixTargetCommand<undefined> {
	constructor(readonly key: string) {
		super();
	}

	command(): string {
		return 'clearLiveSet';
	}

	protected args(): unknown[] {
		return [this.key];
	}

	result(responseAll: Buffer[], success: PromiseSuccess<undefined>, error: PromiseError<undefined>): void {
		parseEmptyResponse(responseAll, success, error);
	}
}
