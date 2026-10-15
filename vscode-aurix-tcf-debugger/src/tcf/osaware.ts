/*
 * The `AurixOS` TCF service: AUTOSAR OS awareness read from an ORTI file.
 *
 * The agent parses the ORTI file, resolves every attribute against the loaded symbols and reads
 * the ones that can change. Nothing about the OS is described here: the object types, their
 * attributes and their display names all come from the file, so a MICROSAR file's `vs_ISR` and
 * `vs_SPINLOCK` arrive exactly like the standard OSEK ones.
 */

import { toBuffer, ValidatingCommand } from './tcfutils';

/** How an attribute's value is meant to be read. */
export type OsAttributeKind = 'ctype' | 'enum' | 'string';

export interface OsAttributeInfo {
	Name: string;
	Description: string;
	Kind: OsAttributeKind;
	/** Declared `NAME[]`: one value per logical core (the ORTI 2.3 SMP extension). */
	PerCore: boolean;
	/** The value can change while the target runs, so it is worth re-reading. */
	Live: boolean;
}

export interface OsTypeInfo {
	Name: string;
	Description: string;
	Count: number;
	Attributes: OsAttributeInfo[];
}

export interface OsInfo {
	Loaded: boolean;
	File?: string;
	Implementation?: string;
	/** ORTI semantics version; `2.3` is the unofficial SMP extension. */
	Version?: string;
	Cores?: number;
	Target?: string | null;
	Types?: OsTypeInfo[];
	/** Attributes that have not resolved yet, with the reason. */
	Errors?: string[];
}

export interface OsAttributeValue {
	Name: string;
	/** Logical core, for a per-core attribute. */
	Core?: number;
	/** What to display: an enumerator name, a number, or literal text. */
	Value: string;
	/** The raw integer behind `Value`, when there is one. */
	Number?: number;
	/** The ORTI object this value points at (a task's stack, a spinlock's owner). */
	Link?: string;
	Error?: string;
}

export interface OsObject {
	Name: string;
	Attributes: OsAttributeValue[];
}

export interface OsObjectList {
	Kind: string;
	Objects: OsObject[];
	/** Bus accesses the agent made. Constants and literal text cost none. */
	Reads: number;
}

/** What is running on one core right now. */
export interface OsRunningCore {
	/** Position in the ORTI file. */
	Core: number;
	/** The AUTOSAR core id (`vs_COREID`), which is what a debug target is matched against. */
	CoreId?: number;
	Task?: string;
	Priority?: string;
	Isr?: string;
	LastError?: string;
	TaskError?: string;
}

export interface OsRunning {
	Cores: OsRunningCore[];
	Reads: number;
}

abstract class OsCommand<T> extends ValidatingCommand<T> {
	service(): string {
		return 'AurixOS';
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}`;
	}
}

/** The agent answers with `{}`-shaped JSON; anything else means a version mismatch. */
function asObject(json: any): any {
	return json && typeof json === 'object' ? json : {};
}

export class GetInfoOsCommand extends OsCommand<OsInfo> {
	command(): string {
		return 'getInfo';
	}

	arguments() {
		return undefined;
	}

	cast(json: any): OsInfo {
		const v = asObject(json);
		return { ...v, Loaded: v.Loaded === true };
	}
}

export class LoadOsCommand extends OsCommand<OsInfo> {
	constructor(
		private readonly file: string,
		private readonly target?: string,
	) {
		super();
	}

	command(): string {
		return 'load';
	}

	arguments() {
		return undefined;
	}

	// Positional arguments, and `toBuffer`'s trailing-argument slot drops a falsy value - so a
	// null target has to be written into the word list itself.
	override toBuffer(token: string): Buffer {
		return toBuffer([
			'C', token, this.service(), this.command(),
			JSON.stringify(this.file), JSON.stringify(this.target ?? null),
		]);
	}

	cast(json: any): OsInfo {
		const v = asObject(json);
		return { ...v, Loaded: v.Loaded === true };
	}
}

export class GetObjectsOsCommand extends OsCommand<OsObjectList> {
	constructor(
		private readonly kind: string,
		private readonly name: string | undefined,
		private readonly liveOnly: boolean,
	) {
		super();
	}

	command(): string {
		return 'getObjects';
	}

	arguments() {
		return undefined;
	}

	override toBuffer(token: string): Buffer {
		return toBuffer([
			'C', token, this.service(), this.command(),
			JSON.stringify(this.kind), JSON.stringify(this.name ?? null), JSON.stringify(this.liveOnly),
		]);
	}

	cast(json: any): OsObjectList {
		const v = asObject(json);
		return { Kind: String(v.Kind ?? this.kind), Objects: Array.isArray(v.Objects) ? v.Objects : [], Reads: Number(v.Reads ?? 0) };
	}
}

export class GetRunningOsCommand extends OsCommand<OsRunning> {
	command(): string {
		return 'getRunning';
	}

	arguments() {
		return undefined;
	}

	cast(json: any): OsRunning {
		const v = asObject(json);
		return { Cores: Array.isArray(v.Cores) ? v.Cores : [], Reads: Number(v.Reads ?? 0) };
	}
}


/** One stack's coverage. */
export interface OsStack {
	Name: string;
	Base: number;
	Size: number;
	/** `DOWN` or `UP`: which end keeps the fill pattern. */
	Direction: string;
	Pattern: number;
	/** Bytes ever used. Absent when it could not be measured. */
	Used?: number;
	Free?: number;
	Percent?: number;
	/** Why it could not be measured, e.g. a region the memory protection hides. */
	Error?: string;
}

export interface OsStackList {
	Stacks: OsStack[];
	Reads: number;
}

export class GetStacksOsCommand extends OsCommand<OsStackList> {
	constructor(private readonly names?: string[]) {
		super();
	}

	command(): string {
		return 'getStacks';
	}

	arguments() {
		return undefined;
	}

	override toBuffer(token: string): Buffer {
		return toBuffer(['C', token, this.service(), this.command(), JSON.stringify(this.names ?? null)]);
	}

	cast(json: any): OsStackList {
		const v = asObject(json);
		return { Stacks: Array.isArray(v.Stacks) ? v.Stacks : [], Reads: Number(v.Reads ?? 0) };
	}
}


/** One observed change of the running task on one core. */
export interface OsTransition {
	/** Microseconds since profiling started. */
	T: number;
	Core: number;
	Task: string;
}

/** Time one task occupied one core. */
export interface OsOccupancy {
	Core: number;
	Task: string;
	Us: number;
}

export interface OsProfile {
	Running: boolean;
	PeriodUs?: number;
	/** Microseconds since profiling started, as of this reply. */
	NowUs?: number;
	Transitions?: OsTransition[];
	Totals?: OsOccupancy[];
	/** Transitions that fell out of the agent's ring before this poll. */
	DroppedTransitions?: number;
	/** Samples the agent could not keep up with, and ticks the core thread missed. */
	Dropped?: number;
	Missed?: number;
	Error?: string;
}

export class StartProfileOsCommand extends OsCommand<{ PeriodUs: number; Cores: number }> {
	constructor(private readonly periodUs: number) {
		super();
	}

	command(): string {
		return 'startProfile';
	}

	arguments() {
		return undefined;
	}

	override toBuffer(token: string): Buffer {
		return toBuffer(['C', token, this.service(), this.command(), JSON.stringify(this.periodUs)]);
	}

	cast(json: any): { PeriodUs: number; Cores: number } {
		const v = asObject(json);
		return { PeriodUs: Number(v.PeriodUs ?? 0), Cores: Number(v.Cores ?? 0) };
	}
}

export class StopProfileOsCommand extends OsCommand<{ Stopped: boolean }> {
	command(): string {
		return 'stopProfile';
	}

	arguments() {
		return undefined;
	}

	cast(json: any): { Stopped: boolean } {
		return { Stopped: asObject(json).Stopped === true };
	}
}

export class GetProfileOsCommand extends OsCommand<OsProfile> {
	constructor(private readonly sinceUs: number) {
		super();
	}

	command(): string {
		return 'getProfile';
	}

	arguments() {
		return undefined;
	}

	override toBuffer(token: string): Buffer {
		return toBuffer(['C', token, this.service(), this.command(), JSON.stringify(this.sinceUs)]);
	}

	cast(json: any): OsProfile {
		const v = asObject(json);
		return { ...v, Running: v.Running === true };
	}
}

/**
 * A TCF failure as text.
 *
 * The service rejects with a plain `TCFError` object (`{Code, Format, ...}`), not with an `Error`,
 * so anything that reaches a user through `String(e)` would read `[object Object]`.
 */
export function describeOsError(e: unknown): string {
	if (e instanceof Error) {
		return e.message;
	}
	if (e && typeof e === 'object') {
		const t = e as { Format?: unknown; Code?: unknown };
		if (typeof t.Format === 'string' && t.Format.length > 0) {
			return t.Format;
		}
		if (typeof t.Code === 'number') {
			return `TCF error ${t.Code}`;
		}
		return JSON.stringify(e);
	}
	return String(e);
}

/** A breakpoint restricted to one task. */
export interface TaskFilter {
	task: string;
	/** `task != NAME`: halt in anything *but* that task. */
	negate: boolean;
}

/**
 * Parses a breakpoint condition as a task filter.
 *
 * The condition field is the natural home for this: VS Code already has UI for it (right-click a
 * breakpoint, Edit Breakpoint, Expression), so no separate concept is needed. Accepted forms are
 * `task == NAME`, `task != NAME`, and a bare task name.
 *
 * Anything else returns `undefined` and the breakpoint keeps halting unconditionally - quietly
 * treating an expression we cannot evaluate as "always" is the safe reading, and treating it as a
 * task name would silently ignore what the user wrote.
 */
export function parseTaskFilter(condition: string | undefined): TaskFilter | undefined {
	const text = (condition ?? '').trim();
	if (!text) {
		return undefined;
	}
	const m = /^task\s*(==|!=|=)\s*([A-Za-z_]\w*)$/i.exec(text);
	if (m) {
		return { task: m[2], negate: m[1] === '!=' };
	}
	return /^[A-Za-z_]\w*$/.test(text) ? { task: text, negate: false } : undefined;
}
