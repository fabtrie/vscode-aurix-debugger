/*
 * Aurix TCF Debug Provider
 *
 * This debug adapter is based on the Intel TCF architecture included in this repo,
 * but adapted to the local project APIs (LifetimeDebugSession + TCFClient).
 * 
 * It implements the Debug Adapter Protocol (DAP) surface for AURIX debugging and bridges requests
 * and events to/from the TCF agent via the TCFClient.
 * Key responsibilities include: breakpoint management, thread/context mapping, stack trace and variable, 
 * disassembly, memory access, multi-core coordination, etc.
 */

import * as path from 'path';
import * as fs from 'fs';
import * as fsPromises from 'fs/promises';
import {
	Breakpoint,
	BreakpointEvent,
	ContinuedEvent,
	Event,
	InvalidatedEvent,
	InitializedEvent,
	OutputEvent,
	Scope,
	Source,
	StackFrame,
	StoppedEvent,
	TerminatedEvent,
	Thread,
	Variable,
} from '@vscode/debugadapter';
import { DebugProtocol } from '@vscode/debugprotocol';
import { LifetimeDebugSession } from './lifetimeDebugSession';
import { TCFClient } from './tcfclient';
import {
	BreakpointData,
	BreakpointType,
	ContextSuspendedData,
	DebugCommandStamper,
	InstanceStatusData,
	NEVER_CANCELLED,
	SmallCommandStamper,
	TCFLogger,
} from './tcf-all';
import { GetChildrenRegistersCommand, GetContextRegistersCommand, GetRegistersCommand, SetRegistersCommand } from './tcf/registers';
import { TCL_SERVICE, TclEvalCommand } from './tcf/tcl';
import { AssignExpressionsCommand, CreateExpressionsCommand, GetContextExpressionsCommand } from './tcf/expressions';
import { FindByAddrSymbolsCommand, FindByNameSymbolsCommand, GetChildrenSymbolsCommand, GetContextSymbolsCommand } from './tcf/symbols';
import { encodeVariableValue } from './variables/encode';
import { GetChildrenStackTraceCommand } from './tcf/stacktrace';
import { SetMemoryMapCommand } from './tcf/memorymap';
import { DisassembleDisassemblyCommand, GetCapabilitiesDisassemblyCommand } from './tcf/disassembly';
import { GetMemoryCommand, MemoryErrorAddress, SetMemoryCommand } from './tcf/memory';
import { MapToSourceLineNumbersCommand, TCFCodeAreaLineNumbers } from './tcf/linenumbers';

export interface InternalLaunchArguments {
	debugTCFMessages?: boolean;
	commandToken?: 'debug' | 'default';
	debugUIFocus?: boolean;
}

// We intentionally do not strictly validate these args because this extension
// accepts AURIX-specific properties beyond the upstream Intel schema.
export interface AurixTCFLaunchRequestArguments extends DebugProtocol.LaunchRequestArguments {
	host?: string;
	port?: number;
	timeout?: number;
	debugTCFMessages?: boolean;
	debugUIFocus?: boolean;
	breakpointPrefix?: string;
	pathMapper?: string;
	internal?: InternalLaunchArguments;
	stackTraceDepth?: number;
	stopOnEntry?: boolean;
	program?: string;
	resumeOtherCoresOnEntry?: boolean;
	globals?: string[];

	// AURIX extras (accepted; mostly used by agent CLI layer)
	targetDevice?: string;
	core?: number;
	entryFunction?: string;
	svdFile?: string;
	workspace?: string;
	flashDownload?: boolean;
	globalAccess?: boolean;
	additionalAgentOptions?: string;

	// Per-core ELF paths (PPU/CDSP/SCR) for multi-core memory mappings
	ppuElfPath?: string;
	cdspElfPaths?: string[];
	scrElfPath?: string;

	// Optional reset behavior (no GUI; can be set in launch.json)
	resetType?: string;
	resetSuspend?: boolean;

	// Mitigation for multi-core "stop storms": delay pause-type stopped events slightly so a breakpoint
	// stopped event can arrive first and become the primary UI context.
	stopStormPauseDelayMs?: number;
	refocusOnWrongContext?: boolean;
	instructionStepping?: boolean;
	liveWatchEnabled?: boolean;
	liveWatchIntervalMs?: number;

	// 'aurix-dbg': OpenOCD-compatible server over TAS (allows SFR memory access,
	// register writes, monitor commands and flashing through Tcl).
	agentFlavor?: 'infineon' | 'aurix-dbg';
	enableFlashing?: boolean;
	// Debugger Actions toolbar button; handled in the extension (debuggerActions.ts).
	actions?: unknown[];
}

const DEFAULT_TCF_HOST = 'localhost';
const DEFAULT_TCF_PORT = 1534;
const DEFAULT_BREAKPOINTS_PREFIX = 'aurix-';
const BREAKPOINT_INVALID = -1;
const MEMORY_READ_FALLBACK_MIN_BYTES = 8;
const MEMORY_READ_FALLBACK_MAX_BYTES = 512;
const MEMORY_READ_FALLBACK_CHUNK_BYTES = 4;

enum BreakpointEventReason {
	removed = 'removed',
	changed = 'changed',
}

enum StoppedEventReason {
	breakpoint = 'breakpoint',
	pause = 'pause',
	entry = 'entry',
	step = 'step',
}

class DebugLogger implements TCFLogger {
	private debugTCF = false;
	private prefix = '';

	constructor(private session: LifetimeDebugSession) { }

	setDebugTCFMessages(debugTCF: boolean) {
		this.debugTCF = debugTCF;
	}

	setPrefix(prefix: string) {
		this.prefix = prefix;
	}

	private time(): string {
		const d = new Date();
		return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}:${d.getSeconds().toString().padStart(2, '0')}`;
	}

	received(message: string): void {
		if (this.debugTCF) {
			this.log(message);
		}
	}

	sent(message: string): void {
		if (this.debugTCF) {
			this.log(message);
		}
	}

	log(message: string): void {
		if (this.debugTCF) {
			this.session.sendEvent(new OutputEvent(`${this.prefix}${this.time()} ${message}\n`, 'console'));
		}
	}

	error(message: string): void {
		if (this.debugTCF) {
			this.session.sendEvent(new OutputEvent(`${this.prefix}${this.time()} ${message}\n`, 'stderr'));
		}
	}

	stdout(message: string): void {
		this.session.sendEvent(new OutputEvent(message, 'stdout'));
	}
}

type BreakpointTuple = { id: number; file: string; line: number; verified: boolean };
type InstructionBreakpointTuple = { id: number; address: number; verified: boolean };
type WatchExpressionState = { result: string; liveRefreshable: boolean };

class BreakpointsManager {
	configured = false;
	private counter = 1;
	private byFile = new Map<string, Map<number, BreakpointTuple>>();
	private byId = new Map<number, BreakpointTuple>();

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

	deleteById(id: number) {
		const b = this.byId.get(id);
		if (!b) {
			return;
		}
		this.byId.delete(id);
		this.byFile.get(b.file)?.delete(b.line);
	}

	updateFile(file: string, lines: number[]): BreakpointTuple[] {
		let map = this.byFile.get(file);
		if (!map) {
			map = new Map();
			this.byFile.set(file, map);
		}

		const nextLines = new Set(lines);
		// Remove deleted lines
		for (const [line, tuple] of Array.from(map.entries())) {
			if (!nextLines.has(line)) {
				map.delete(line);
				this.byId.delete(tuple.id);
			}
		}
		// Add new lines
		for (const line of lines) {
			if (!map.has(line)) {
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

class InstructionBreakpointsManager {
	private byAddress = new Map<number, InstructionBreakpointTuple>();
	private byId = new Map<number, InstructionBreakpointTuple>();

	setVerified(id: number, verified: boolean) {
		const b = this.byId.get(id);
		if (b) {
			b.verified = verified;
		}
	}

	deleteById(id: number) {
		const b = this.byId.get(id);
		if (!b) {
			return;
		}
		this.byId.delete(id);
		this.byAddress.delete(b.address);
	}

	updateAddresses(addresses: number[], allocateId: () => number): InstructionBreakpointTuple[] {
		const next = new Set(addresses);
		// Remove deleted addresses
		for (const [addr, tuple] of Array.from(this.byAddress.entries())) {
			if (!next.has(addr)) {
				this.byAddress.delete(addr);
				this.byId.delete(tuple.id);
			}
		}
		// Add new addresses
		for (const addr of addresses) {
			if (!this.byAddress.has(addr)) {
				const tuple: InstructionBreakpointTuple = { id: allocateId(), address: addr, verified: false };
				this.byAddress.set(addr, tuple);
				this.byId.set(tuple.id, tuple);
			}
		}
		return Array.from(this.byAddress.values()).sort((a, b) => a.address - b.address);
	}

	getAll(): InstructionBreakpointTuple[] {
		return Array.from(this.byId.values());
	}

	getById(id: number): InstructionBreakpointTuple | undefined {
		return this.byId.get(id);
	}
}

/** Where the Disassembly view shows the opcode bytes of an instruction. */
export type DisassemblyOpcodeBytes = 'left' | 'right' | 'hidden';

export function isDisassemblyOpcodeBytes(value: unknown): value is DisassemblyOpcodeBytes {
	return value === 'left' || value === 'right' || value === 'hidden';
}

/** "xx xx xx xx": TriCore instructions are at most 4 bytes. */
const DISASSEMBLY_BYTES_WIDTH = 11;
/** Instruction text width before the opcode bytes when they are shown on the right. */
const DISASSEMBLY_INSTRUCTION_WIDTH = 32;
/** Gap between the opcode bytes and the instruction text. */
const DISASSEMBLY_COLUMN_GAP = ' '.repeat(5);

export class AurixTCFDebugSession extends LifetimeDebugSession {
	private enabledPathMapper: boolean;
	private stackTraceDepth: number = -1;
	private instructionSteppingEnabled = false;
	private execRanges: Array<{ start: number; end: number }> = [];
	private breakpointPrefix = DEFAULT_BREAKPOINTS_PREFIX;
	private pathMapper: (p: string) => string = (p) => p;
	private sourceResolverBaseDir: string | undefined;
	private workspaceBaseDir: string | undefined;
	private projectRootDir: string | undefined;
	private librariesBasenameIndex: Map<string, string> | undefined;
	private programPath: string | undefined;
	private disassemblyCapabilities: string[] | undefined;
	private disassemblyCapabilitiesLogged = false;
	/**
	 * Address picked with "Go to Address or Symbol". VS Code's Disassembly view can only be moved by
	 * focusing a stack frame, so the target is shown as an extra frame at the bottom of the suspended
	 * cores' call stacks until a core resumes. It is added to every core because refreshing the call
	 * stacks may move the focus to another core.
	 */
	private disassemblyJumpTarget: { address: number; label: string } | undefined;
	private elfFunctionSymbols: Array<{ name: string; address: number }> | undefined;
	private disassemblyOpcodeBytes: DisassemblyOpcodeBytes;

	private breakpointsManager = new BreakpointsManager();
	private instructionBreakpointsManager = new InstructionBreakpointsManager();

	private contextToThreadId = new Map<string, number>();
	private threadIdToContext = new Map<number, string>();

	private tcfLogger: DebugLogger;
	private tcfClient: TCFClient;
	private suspendedContextIds = new Set<string>();
	private pendingStepThreads = new Set<number>();
	private configuredGlobals: string[] = [];
	private liveWatchEnabled = false;
	private liveWatchIntervalMs = 1000;
	private agentFlavor: 'infineon' | 'aurix-dbg' = 'infineon';
	private liveWatchTimer: ReturnType<typeof setInterval> | undefined;
	private liveWatchRefreshInFlight = false;
	private watchExpressionStates = new Map<string, WatchExpressionState>();
	private launchArgs: AurixTCFLaunchRequestArguments | undefined;
	private launchCore: number | undefined;
	private launchResetType: string | undefined;
	private launchResetSuspend: boolean = true;
	private startupDiagnosticsEnabled: boolean = false;
	private uiFocusDiagnosticsEnabled: boolean = false;
	private stopStormPauseDelayMs: number = 30;
	private stopStormStackSuppressMs: number = 500;
	private pendingPauseStoppedTimers = new Map<string, ReturnType<typeof setTimeout>>();
	private pendingBreakpointStoppedTimers = new Map<string, ReturnType<typeof setTimeout>>();
	private breakpointStopDelayMs: number = 60;
	private suppressPauseStoppedUntilMs: number = 0;
	// Thread VS Code has focused (aurix-tcf/activeThread from the extension). VS Code's toolbar pause
	// button pauses the focused thread; a pause for any other thread comes from its Call Stack entry.
	private activeThreadId: number | undefined;
	// Explicit pause in progress: suspend events of the cores TL1 stopped with it must not take the
	// focus, only the core chosen by pauseRequestAsync.
	private userPauseUntilMs: number = 0;
	private refocusOnWrongContext: boolean = true;
	private primaryBreakpointStop:
		| { threadId: number; ctx: string; untilMs: number; refocused: boolean; stackSuppressUntilMs: number }
		| undefined;
	private frameIdToMeta = new Map<number, { threadId: number; ctx: string; stack?: string }>();
	private threadIdToFrameIds = new Map<number, Set<number>>();
	private liveWatchSuspendFallbackBlockedUntilMs: number = 0;
	private postResetStackTrimUntilMs: number = 0;
	private cachedCoreContextId: string | undefined;
	private cachedCoreContextKey: number | undefined | null = null; // null = not yet cached

	constructor(enablePathMapper: boolean, disassemblyOpcodeBytes: DisassemblyOpcodeBytes = 'left') {
		super();
		this.enabledPathMapper = enablePathMapper;
		this.disassemblyOpcodeBytes = disassemblyOpcodeBytes;
		this.tcfLogger = new DebugLogger(this);

		const outer = this;
		this.tcfClient = new class extends TCFClient {
			constructor(logger: TCFLogger) {
				super(logger, new SmallCommandStamper());
			}

			setCommandToken(kind: string) {
				this.tokenIdGenerator = kind === 'debug' ? new DebugCommandStamper() : new SmallCommandStamper();
			}

			protected onBreakpoint(breakpointID: string, status: InstanceStatusData[]): void {
				outer.onBreakpointHit(breakpointID, status);
			}
			protected onBreakpointRemoved(breakpointIDs: string[]): void {
				outer.onBreakpointRemoved(breakpointIDs);
			}
			protected onBreakpointAdded(breakpointIDs: string[]): void {
				outer.onBreakpointAddedOrChanged(breakpointIDs);
			}
			protected onBreakpointChanged(breakpointIDs: string[]): void {
				outer.onBreakpointAddedOrChanged(breakpointIDs);
			}
			protected onRunControlContextSuspended(info: ContextSuspendedData) {
				outer.onContextSuspended(info);
			}
			protected onRunControlContextResumed(contextID: string) {
				outer.onContextResumed(contextID);
			}
			protected onSocketError(err: any) {
				outer.tcfLogger.error(`TCF socket error: ${String(err)}`);
				outer.sendEvent(new TerminatedEvent());
			}
		}(this.tcfLogger);

		this.setDebuggerLinesStartAt1(true);
		this.setDebuggerColumnsStartAt1(true);
	}

	private focusLog(message: string) {
		if (!this.uiFocusDiagnosticsEnabled) {
			return;
		}
		this.sendEvent(new OutputEvent(`[focus] ${message}\n`, 'console'));
		try {
			const dir = process.env.TEMP || process.env.TMPDIR || '/tmp';
			fs.appendFileSync(path.join(dir, 'aurix-tcf-focus.log'), `${new Date().toISOString()} [adapter] ${message}\n`);
		} catch {
			// diagnostics only
		}
	}

	private startupLog(message: string) {
		if (!this.startupDiagnosticsEnabled) {
			return;
		}
		this.tcfLogger.log(`[startup] ${message}`);
	}

	private formatThreadsSnapshot(infos: any[]): string {
		return infos
			.map(t => {
				const id = String(t?.info?.ID ?? '');
				const name = t?.info?.Name ? String(t.info.Name) : '';
				const suspended = Boolean(t?.state?.suspended);
				return `${id}${name ? `(${name})` : ''}:${suspended ? 'S' : 'R'}`;
			})
			.filter(s => s.trim() !== '')
			.join(', ');
	}

	private async startupLogThreads(tag: string): Promise<void> {
		if (!this.startupDiagnosticsEnabled) {
			return;
		}
		try {
			const infos = await this.tcfClient.getThreads();
			this.tcfLogger.log(`[startup] ${tag}: threads=${infos.length}${infos.length ? ` ${this.formatThreadsSnapshot(infos)}` : ''}`);
		} catch (e) {
			this.tcfLogger.log(`[startup] ${tag}: failed to query threads: ${String(e)}`);
		}
	}

	private async pauseAllCoresBestEffort(tag: string): Promise<void> {
		// Query all threads/contexts and pause any that are running.
		// Retries a second time to catch any cores that were still starting up.
		// Used for pauseOnEntry and reset+halt scenarios.
		try {
			for (let attempt = 0; attempt < 2; attempt++) {
				const infos = await this.tcfClient.getThreads();
				const running = infos.filter(t => !t.state?.suspended).map(t => t.info.ID);
				const suspended = infos.filter(t => Boolean(t.state?.suspended)).map(t => t.info.ID);
				this.startupLog(
					`pauseAllCoresBestEffort(${tag}) attempt=${attempt + 1}: running=${running.length ? running.join(', ') : '(none)'} suspended=${suspended.length ? suspended.join(', ') : '(none)'}`
				);

				if (running.length === 0) {
					return;
				}

				for (const id of running) {
					try {
						await this.tcfClient.pause(id);
					} catch {
						// Best-effort: tolerate agent quirks/timeouts for individual cores.
					}
				}

				// Give the agent a moment to publish updated states/events.
				await new Promise(resolve => setTimeout(resolve, 150));
			}
		} catch (e) {
			this.tcfLogger.error(`pauseAllCoresBestEffort(${tag}) failed: ${String(e)}`);
		}
	}

	private sendStoppedEvent(
		reason: string,
		threadId: number,
		opts?: { allThreadsStopped?: boolean; preserveFocus?: boolean }
	) {
		const ctx = this.threadIdToContext.get(threadId);
		this.focusLog(
			`StoppedEvent: reason=${reason} threadId=${threadId} ctx=${String(ctx ?? '')} allThreadsStopped=${String(Boolean(opts?.allThreadsStopped))} preserveFocusHint=${String(opts?.preserveFocus)}`
		);
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(
				`[dap] StoppedEvent reason=${reason} threadId=${threadId} ctx=${String(ctx ?? '')} allThreadsStopped=${String(Boolean(opts?.allThreadsStopped))}`
			);
		}
		const ev = new StoppedEvent(reason, threadId);
		if (opts?.allThreadsStopped) {
			(ev.body as any).allThreadsStopped = true;
		}
		if (opts?.preserveFocus !== undefined) {
			(ev.body as DebugProtocol.StoppedEvent['body']).preserveFocusHint = opts.preserveFocus;
		}
		this.sendEvent(ev);
	}

	private cancelPendingPauseStoppedTimers(tag: string) {
		if (this.pendingPauseStoppedTimers.size === 0) {
			return;
		}
		for (const t of this.pendingPauseStoppedTimers.values()) {
			clearTimeout(t);
		}
		this.pendingPauseStoppedTimers.clear();
		this.focusLog(`cancelPendingPauseStoppedTimers(${tag})`);
	}

	private cancelPendingBreakpointStoppedTimer(contextId: string, tag: string) {
		const timer = this.pendingBreakpointStoppedTimers.get(contextId);
		if (!timer) {
			return;
		}
		clearTimeout(timer);
		this.pendingBreakpointStoppedTimers.delete(contextId);
		this.focusLog(`cancelPendingBreakpointStoppedTimer(${contextId}, ${tag})`);
	}

	private notePrimaryBreakpointStop(threadId: number, ctx: string) {
		this.primaryBreakpointStop = {
			threadId,
			ctx,
			untilMs: Date.now() + 2_000,
			refocused: false,
			stackSuppressUntilMs: Date.now() + this.stopStormStackSuppressMs,
		};
	}

	private maybeRefocusToBreakpointThread(from: string, requested: { threadId?: number; ctx?: string }) {
		if (!this.refocusOnWrongContext) {
			return;
		}
		const p = this.primaryBreakpointStop;
		if (!p) {
			return;
		}
		if (Date.now() > p.untilMs) {
			return;
		}
		if (p.refocused) {
			return;
		}
		if (requested.threadId === undefined || requested.ctx === undefined) {
			return;
		}
		if (requested.threadId === p.threadId) {
			return;
		}

		p.refocused = true;
		this.tcfLogger.log(
			`[dap] Refocus: from=${from} requestedThreadId=${requested.threadId} requestedCtx=${requested.ctx} -> breakpointThreadId=${p.threadId} breakpointCtx=${p.ctx}`
		);
		this.sendStoppedEvent(StoppedEventReason.breakpoint, p.threadId, { allThreadsStopped: true });
	}

	private tryResolveFrame(frameId: number): { threadId?: number; ctx?: string; stack?: string } {
		const cached = this.frameIdToMeta.get(frameId);
		if (cached) {
			return { threadId: cached.threadId, ctx: cached.ctx, stack: cached.stack };
		}
		try {
			const localRef = this.getLocalScopeVariableReference(frameId);
			const [ctx, stack] = this.getStackFrameDetails(localRef);
			const threadId = this.contextToThreadId.get(ctx) ?? this.getOrAssignThreadId(ctx);
			return { threadId, ctx, stack };
		} catch {
			return {};
		}
	}

	private rememberFrameMeta(frameId: number, threadId: number, ctx: string, stack?: string) {
		this.frameIdToMeta.set(frameId, { threadId, ctx, stack });
		let set = this.threadIdToFrameIds.get(threadId);
		if (!set) {
			set = new Set<number>();
			this.threadIdToFrameIds.set(threadId, set);
		}
		set.add(frameId);
	}

	private clearFrameMetaForThread(threadId: number) {
		const ids = this.threadIdToFrameIds.get(threadId);
		if (!ids) {
			return;
		}
		for (const frameId of ids.values()) {
			this.frameIdToMeta.delete(frameId);
		}
		this.threadIdToFrameIds.delete(threadId);
	}

	private clearAllFrameMeta(): void {
		for (const threadId of Array.from(this.threadIdToFrameIds.keys())) {
			this.clearFrameMetaForThread(threadId);
		}
	}

	private beginResetTransition(): void {
		const now = Date.now();
		this.liveWatchSuspendFallbackBlockedUntilMs = now + 1500;
		this.postResetStackTrimUntilMs = now + 2000;
		this.primaryBreakpointStop = undefined;
		this.suppressPauseStoppedUntilMs = 0;
		this.cancelPendingPauseStoppedTimers('reset');
		for (const contextId of Array.from(this.pendingBreakpointStoppedTimers.keys())) {
			this.cancelPendingBreakpointStoppedTimer(contextId, 'reset');
		}
		this.clearAllFrameMeta();
	}

	private trimPostResetStack<T extends { mapToSource?: TCFCodeAreaLineNumbers }>(stack: T[]): T[] {
		if (Date.now() > this.postResetStackTrimUntilMs || stack.length <= 1) {
			return stack;
		}
		const trimmed: T[] = [stack[0]];
		for (const frame of stack.slice(1)) {
			if (frame?.mapToSource?.File && frame?.mapToSource?.SLine) {
				trimmed.push(frame);
				continue;
			}
			break;
		}
		return trimmed;
	}

	protected override onThreadSuspended(thread: number) {
		this.clearFrameMetaForThread(thread);
		super.onThreadSuspended(thread);
	}

	protected override onThreadResumed(thread: number) {
		this.clearFrameMetaForThread(thread);
		super.onThreadResumed(thread);
	}

	private getOrAssignThreadId(contextId: string): number {
		// Core contextId -> threadId mapping to keep thread IDs stable across sessions.
		// TCF agent context IDs: S0.D0.C0, S0.D0.C1, S0.D0.C2, ...
		// VS Code needs numeric thread IDs.
		// For example, core 0 and core 1 paused at the same time and first event arrives for core 1: we still want core 0 to be thread 1.
		const existing = this.contextToThreadId.get(contextId);
		if (existing) {
			this.threadIdToContext.set(existing, contextId);
			return existing;
		}

		// Deterministic thread IDs for AURIX multi-core contexts. This reduces UI "core highlight"
		// instability caused by threadId ordering depending on event arrival order.
		// Examples: S0.D0.C0 -> 1, S0.D0.C1 -> 2, S0.D0.C2 -> 3.
		let desiredId: number | undefined;
		const m = /(?:^|\.)C(\d+)(?:$|\b)/i.exec(contextId);
		if (m) {
			const coreNum = Number(m[1]);
			if (Number.isFinite(coreNum) && coreNum >= 0) {
				desiredId = coreNum + 1;
			}
		}

		let id: number;
		if (desiredId !== undefined && desiredId > 0) {
			const occupiedBy = this.threadIdToContext.get(desiredId);
			if (!occupiedBy || occupiedBy === contextId) {
				id = desiredId;
			} else {
				// Collision (unexpected): fall back to next free ID.
				id = 1;
				while (this.threadIdToContext.has(id)) {
					id++;
				}
			}
		} else {
			id = 1;
			while (this.threadIdToContext.has(id)) {
				id++;
			}
		}

		this.contextToThreadId.set(contextId, id);
		this.threadIdToContext.set(id, contextId);
		return id;
	}

	protected override convertClientPathToDebugger(p: string): string {
		const mapped = this.pathMapper(p);
		// The TCF side commonly uses '/' path separators even on Windows.
		return mapped.replace(/\\/g, '/');
	}

	protected override convertDebuggerPathToClient(p: string): string {
		const normalized = p.replace(/\\/g, '/');
		const isWindowsAbs = /^[a-zA-Z]:\//.test(normalized);
		const isAbs = path.isAbsolute(normalized) || isWindowsAbs;
		if (isAbs) {
			// Keep the absolute path shape but normalize separators for local FS.
			const localAbs = isWindowsAbs ? normalized.replace(/\//g, '\\') : normalized;
			if (fs.existsSync(localAbs)) {
				return localAbs;
			}

			// Fix: some toolchains embed absolute paths from a different machine/workspace
			// (e.g. "C:\\Infineon\\...\\TC375_CE\\Libraries\\..."). If that path doesn't exist locally,
			// remap it onto the detected local project root.
			const lower = normalized.toLowerCase();
			const tcMarker = '/tc375_ce/';
			const libMarker = '/libraries/';
			const candidates: string[] = [];
			if (this.projectRootDir) {
				const tcIndex = lower.indexOf(tcMarker);
				if (tcIndex >= 0) {
					const tail = normalized.substring(tcIndex + tcMarker.length);
					candidates.push(path.resolve(this.projectRootDir, ...tail.split('/')));
				}
				const libIndex = lower.indexOf(libMarker);
				if (libIndex >= 0) {
					const tail = normalized.substring(libIndex + 1); // "Libraries/..."
					candidates.push(path.resolve(this.projectRootDir, ...tail.split('/')));
				}
			}
			for (const c of candidates) {
				if (fs.existsSync(c)) {
					return c;
				}
			}

			return localAbs;
		}

		// Fix: debug info often reports project-relative paths (e.g. "Libraries/..."),
		// so resolve them to an existing local file by probing a few likely roots.
		const candidateBases: string[] = [];
		for (const base of [this.workspaceBaseDir, this.sourceResolverBaseDir].filter(Boolean) as string[]) {
			candidateBases.push(base);
		}
		// Also try parent folders of the ELF directory (common: <project>/TriCore Debug (GCC)/<elf>).
		// This helps open headers like "Libraries/..." when args.workspace is not the project root.
		if (this.sourceResolverBaseDir) {
			let dir = this.sourceResolverBaseDir;
			for (let i = 0; i < 6; i++) {
				dir = path.dirname(dir);
				candidateBases.push(dir);
				if (dir === path.dirname(dir)) {
					break;
				}
			}
		}
		// De-dup while keeping order.
		const seen = new Set<string>();
		const uniqueBases = candidateBases.filter((b) => {
			if (!b) {
				return false;
			}
			const key = path.resolve(b);
			if (seen.has(key)) {
				return false;
			}
			seen.add(key);
			return true;
		});
		for (const base of uniqueBases) {
			const candidate = path.resolve(base, normalized);
			if (fs.existsSync(candidate)) {
				return candidate;
			}
		}

		// Fix: some LineNumbers providers return only a basename (e.g. "Ifx_Ssw_CompilersGcc.h")
		// with no directory info. In that case, try to locate it under <projectRoot>/Libraries.
		if (this.projectRootDir && !normalized.includes('/') && normalized.trim() !== '') {
			const found = this.lookupLibrariesFileByBasename(normalized);
			if (found) {
				return found;
			}
		}
		return normalized;
	}

	private lookupLibrariesFileByBasename(basename: string): string | undefined {
		if (!this.projectRootDir) {
			return undefined;
		}
		if (!this.librariesBasenameIndex) {
			this.librariesBasenameIndex = new Map<string, string>();

			// Best-effort: index a limited number of files so this never blocks the debug adapter.
			// We only scan under "Libraries" because that's where missing headers typically live.
			const root = path.join(this.projectRootDir, 'Libraries');
			try {
				if (fs.existsSync(root) && fs.statSync(root).isDirectory()) {
					const stack: string[] = [root];
					let visited = 0;
					const maxVisited = 30000;
					while (stack.length > 0 && visited < maxVisited) {
						const dir = stack.pop()!;
						let entries: fs.Dirent[];
						try {
							entries = fs.readdirSync(dir, { withFileTypes: true });
						} catch {
							continue;
						}
						for (const ent of entries) {
							visited++;
							if (visited >= maxVisited) {
								break;
							}
							const full = path.join(dir, ent.name);
							if (ent.isDirectory()) {
								stack.push(full);
								continue;
							}
							if (!ent.isFile()) {
								continue;
							}
							const ext = path.extname(ent.name).toLowerCase();
							if (ext !== '.h' && ext !== '.hpp' && ext !== '.c' && ext !== '.cpp') {
								continue;
							}
							// Prefer the first hit to keep it deterministic.
							if (!this.librariesBasenameIndex.has(ent.name)) {
								this.librariesBasenameIndex.set(ent.name, full);
							}
						}
					}
				}
			} catch {
				// ignore
			}
		}

		return this.librariesBasenameIndex.get(basename);
	}

	private resolveMappedSourcePath(mapped: TCFCodeAreaLineNumbers): string | undefined {
		if (!mapped.File) {
			return undefined;
		}
		let fileFromDwarf: string = mapped.File;
		if (mapped.Dir && mapped.Dir.trim() !== '' && !path.isAbsolute(fileFromDwarf) && !/^[a-zA-Z]:[\\/]/.test(fileFromDwarf)) {
			const dir = mapped.Dir;
			const looksWindows = /^[a-zA-Z]:[\\/]/.test(dir) || dir.includes('\\');
			const joinedCandidate = looksWindows
				? path.win32.normalize(path.win32.join(dir, fileFromDwarf))
				: path.posix.normalize(path.posix.join(dir.replace(/\\/g, '/'), fileFromDwarf.replace(/\\/g, '/')));
			if (joinedCandidate) {
				if (looksWindows || fs.existsSync(joinedCandidate)) {
					fileFromDwarf = joinedCandidate;
				}
			}
		}
		return this.convertDebuggerPathToClient(fileFromDwarf);
	}

	private async initDefaultPathMapper(args: AurixTCFLaunchRequestArguments): Promise<void> {
		// If the user didn't provide a custom mapper, default to a DWARF-typical
		// layout: sources referenced relative to the build output directory.
		//
		// For AURIX ADS/GCC projects, the compiler commonly invokes sources like
		// "../Cpu0_Main.c" from the build directory (where the .elf lives). In that
		// case, sending an absolute Windows path to TCF Breakpoints/LineNumbers will
		// produce "No debug information for this source line" even when DWARF exists.
		if (args.pathMapper && args.pathMapper.trim() !== '') {
			return;
		}
		if (!args.program || args.program.trim() === '') {
			return;
		}

		const programDir = path.dirname(args.program);
		this.sourceResolverBaseDir = programDir;

		// Map client paths to paths relative to the .elf directory.
		// Keep already-relative paths as-is.
		this.pathMapper = (p: string) => {
			if (!p) {
				return p;
			}

			const normalized = p.replace(/\\/g, '/');
			const alreadyRelative = !path.isAbsolute(normalized) && !/^[a-zA-Z]:\//.test(normalized);
			if (alreadyRelative) {
				return normalized;
			}

			try {
				// Use OS-specific path APIs for correctness (drive letters, etc.)
				const rel = path.relative(programDir, p);
				// If drives differ, path.relative returns an absolute-ish path containing ':'; ignore it.
				if (!rel || /^[a-zA-Z]:/.test(rel) || path.isAbsolute(rel)) {
					return normalized;
				}
				return rel.replace(/\\/g, '/');
			} catch {
				return normalized;
			}
		};
	}

	/**
	 * Detect core type from a RunControl context ID or Name.
	 * Returns { type, index } or undefined if it's a plain TriCore context.
	 */
	private static extractCoreType(idOrName: string): { type: 'PPU' | 'CDSP' | 'SCR'; index: number } | undefined {
		if (/\bPPU\b/i.test(idOrName)) {
			return { type: 'PPU', index: 0 };
		}
		const cdspMatch = idOrName.match(/\bCDSP(\d+)\b/i);
		if (cdspMatch) {
			return { type: 'CDSP', index: parseInt(cdspMatch[1], 10) };
		}
		if (/\bSCR\b/i.test(idOrName)) {
			return { type: 'SCR', index: 0 };
		}
		return undefined;
	}

	/**
	 * Resolve the ELF path for a given core type, falling back to the main program ELF.
	 */
	private getElfPathForCore(
		coreInfo: { type: 'PPU' | 'CDSP' | 'SCR'; index: number } | undefined,
		args: AurixTCFLaunchRequestArguments,
	): string | undefined {
		if (coreInfo) {
			switch (coreInfo.type) {
				case 'PPU':
					if (args.ppuElfPath && args.ppuElfPath.trim() !== '') { return args.ppuElfPath; }
					break;
				case 'CDSP':
					if (Array.isArray(args.cdspElfPaths) && args.cdspElfPaths[coreInfo.index]?.trim()) {
						return args.cdspElfPaths[coreInfo.index];
					}
					break;
				case 'SCR':
					if (args.scrElfPath && args.scrElfPath.trim() !== '') { return args.scrElfPath; }
					break;
			}
		}
		// Fallback to main TriCore ELF
		return args.program && args.program.trim() !== '' ? args.program : undefined;
	}

	private async initMemoryMap(args: AurixTCFLaunchRequestArguments): Promise<void> {
		// Enumerate leaf execution contexts (HasState === true) from the agent,
		// identify each core type (PPU/CDSPx/SCR/TriCore), and set a MemoryMap
		// entry pointing to the core-specific ELF.
		// Only leaf contexts with HasState are valid MemoryMap targets; container
		// contexts (e.g. S0, S0.D0) must be skipped or the agent may reset the connection.
		try {
			const threads = await this.tcfClient.getThreads();
			if (threads.length === 0) {
				this.tcfLogger.log('initMemoryMap: no execution contexts found');
				return;
			}

			let mappedCount = 0;
			for (const thread of threads) {
				const contextId = thread.info.ID;
				const contextName = thread.info.Name ?? '';
				// Try ID first, then Name for core type detection
				const coreInfo = AurixTCFDebugSession.extractCoreType(contextId)
					?? AurixTCFDebugSession.extractCoreType(contextName);
				const elfPath = this.getElfPathForCore(coreInfo, args);
				if (!elfPath) {
					continue;
				}

				// Verify the file exists locally before trying to set the mapping.
				try {
					const st = await fsPromises.stat(elfPath);
					if (!st.isFile()) { continue; }
				} catch {
					continue;
				}

				const fileName = elfPath.replace(/\\/g, '/');
				const coreLabel = coreInfo ? `${coreInfo.type}${coreInfo.type === 'CDSP' ? coreInfo.index : ''}` : 'TriCore';
				try {
					await this.tcfClient.sendCommand(
						new SetMemoryMapCommand(contextId, [
							{ ['FileName']: fileName, ['ID']: contextId },
						])
					);
					mappedCount++;
					this.tcfLogger.log(`MemoryMap: ${contextId} (${coreLabel}) → ${fileName}`);
				} catch (e) {
					this.tcfLogger.error(`MemoryMap failed for ${contextId} (${coreLabel}): ${String(e)}`);
				}
			}

			this.tcfLogger.log(`MemoryMap initialized: ${mappedCount} context(s) mapped`);
		} catch (e) {
			this.tcfLogger.error(`MemoryMap initialization failed: ${String(e)}`);
		}
	}

	private async pickContextIdForCore(core: number | undefined): Promise<string | undefined> {
		// Pick a context ID matching the requested core number.
		// If core is undefined, pick the first available context.
		// If no matching core is found, fall back to the first available context.
		// For example, core=0 -> S0.D0.C0

		// Use cached result when available (core-to-context mapping is stable within a session).
		const coreKey = core ?? undefined;
		if (this.cachedCoreContextKey === coreKey && this.cachedCoreContextId !== undefined) {
			return this.cachedCoreContextId;
		}

		const infos = await this.tcfClient.getThreads();
		if (infos.length === 0) {
			this.startupLog(`pickContextIdForCore(core=${String(core)}): no threads from agent`);
			return undefined;
		}
		if (core === undefined || core === null) {
			this.startupLog(`pickContextIdForCore(core=undefined): picked first=${infos[0].info.ID}`);
			this.cachedCoreContextKey = undefined;
			this.cachedCoreContextId = infos[0].info.ID;
			return this.cachedCoreContextId;
		}

		const idRe = new RegExp(`(?:^|\\.)C${core}(?:$|\\b)`, 'i');
		const nameRe = new RegExp(`\\bcore\\s*${core}\\b|\\bC${core}\\b`, 'i');
		const found = infos.find(t => idRe.test(t.info.ID) || (t.info.Name ? nameRe.test(t.info.Name) : false));
		const picked = (found ?? infos[0]).info.ID;
		this.startupLog(`pickContextIdForCore(core=${core}): picked=${picked}${found ? '' : ' (fallback)'}`);
		this.focusLog(`pickContextIdForCore(core=${String(core)}): picked=${picked}${found ? ' (matched)' : ' (fallback)'}`);
		this.cachedCoreContextKey = coreKey;
		this.cachedCoreContextId = picked;
		return picked;
	}

	private async emitStoppedIfSuspended(contextId: string, reason: StoppedEventReason): Promise<void> {
		const infos = await this.tcfClient.getThreads();
		const match = infos.find(t => t.info.ID === contextId);
		if (!match) {
			this.startupLog(`emitStoppedIfSuspended(${contextId}, reason=${reason}): context not found; ${this.formatThreadsSnapshot(infos)}`);
			return;
		}
		if (!match.state?.suspended) {
			this.startupLog(`emitStoppedIfSuspended(${contextId}, reason=${reason}): not suspended`);
			return;
		}
		const threadId = this.getOrAssignThreadId(contextId);
		this.onThreadSuspended(threadId);
		this.sendStoppedEvent(reason, threadId);
		this.startupLog(`emitStoppedIfSuspended(${contextId}, reason=${reason}): emitted StoppedEvent(threadId=${threadId})`);
	}

	private async emitSourceLocation(contextId: string): Promise<void> {
		try {
			const findMapped = (frames: Array<{ mapToSource?: TCFCodeAreaLineNumbers }>): TCFCodeAreaLineNumbers | undefined => {
				for (const frame of frames) {
					if (frame?.mapToSource?.File && frame.mapToSource.SLine) {
						return frame.mapToSource;
					}
				}
				return undefined;
			};

			// Fast path: only query the top frame first.
			let stack = await this.tcfClient.getStackTrace(contextId, NEVER_CANCELLED, 1);
			let mapped = findMapped(stack);

			// Fallback: if top frame has no mapping, query full stack.
			if (!mapped) {
				stack = await this.tcfClient.getStackTrace(contextId, NEVER_CANCELLED, -1);
				mapped = findMapped(stack);
			}
			if (!mapped?.File || !mapped.SLine) {
				return;
			}
			const sourcePath = this.resolveMappedSourcePath(mapped);
			if (!sourcePath) {
				return;
			}
			this.focusLog(`sourceLocation ctx=${contextId} ${sourcePath}:${mapped.SLine}`);
			this.sendEvent(new Event('aurix-tcf/sourceLocation', { path: sourcePath, line: mapped.SLine }));
		} catch (e) {
			if (this.startupDiagnosticsEnabled) {
				this.tcfLogger.log(`[source] failed to resolve source location for ${contextId}: ${String(e)}`);
			}
		}
	}

	private async emitStoppedForAllSuspendedAtStartup(
		// The targetContextId is the one the user requested to stop on (if any).
		// We prefer to emit the stopped event for that core if it's suspended,
		// otherwise we pick any other suspended core.
		// This helps keep the UI stable during startup (no core switching).
		targetContextId: string | undefined,
		opts?: { targetReason?: StoppedEventReason; otherReason?: StoppedEventReason }
	): Promise<void> {
		try {
			const infos = await this.tcfClient.getThreads();
			const suspended = infos.filter(t => Boolean(t.state?.suspended)).map(t => t.info.ID);
			this.startupLog(`emitStoppedForAllSuspendedAtStartup: suspended=${suspended.length ? suspended.join(', ') : '(none)'}`);
			const targetReason = opts?.targetReason ?? StoppedEventReason.entry;

			// Mark all suspended cores in our internal state and invalidate per-thread cached references.
			for (const ctx of suspended) {
				const threadId = this.getOrAssignThreadId(ctx);
				this.suspendedContextIds.add(ctx);
				this.onThreadSuspended(threadId);
			}

			// DAP: Emit a single stopped event with allThreadsStopped=true. This keeps the UI stable and
			// prevents VS Code from opening sources for other cores during startup.
			let focusCtx = targetContextId;
			if (!focusCtx || !suspended.includes(focusCtx)) {
				focusCtx = suspended.length > 0 ? suspended[0] : undefined;
			}
			if (focusCtx) {
				const threadId = this.getOrAssignThreadId(focusCtx);
				this.sendStoppedEvent(targetReason, threadId, { allThreadsStopped: true, preserveFocus: false });
				this.startupLog(`emitStoppedForAllSuspendedAtStartup: emitted ${targetReason} for ${focusCtx} (threadId=${threadId}) allThreadsStopped=true`);
			}
		} catch (e) {
			this.startupLog(`emitStoppedForAllSuspendedAtStartup: failed: ${String(e)}`);
		}
	}

	private async resumeOtherCoresIfNeeded(targetContextId: string | undefined, args: AurixTCFLaunchRequestArguments): Promise<void> {
		// User expectation: stop on one core while other cores keep running.
		if (!args.stopOnEntry || targetContextId === undefined) {
			return;
		}
		if (args.resumeOtherCoresOnEntry !== true) {
			return;
		}

		try {
			const infos = await this.tcfClient.getThreads();
			const othersSuspended = infos
				.filter(t => t.info.ID !== targetContextId)
				.filter(t => Boolean(t.state?.suspended))
				.map(t => t.info.ID);
			this.startupLog(`resumeOtherCoresOnEntry: target=${targetContextId}, othersSuspended=${othersSuspended.length ? othersSuspended.join(', ') : '(none)'}`);

			for (const other of othersSuspended) {
				try {
					await this.tcfClient.continue(other);
					this.startupLog(`resumeOtherCoresOnEntry: continued ${other}`);
				} catch {
					// best-effort; some contexts might not be resumable
					this.startupLog(`resumeOtherCoresOnEntry: failed to continue ${other}`);
				}
			}
		} catch {
			// best-effort only
		}
	}

	private async getRegisterContextChildren(parentId: string): Promise<string[]> {
		return (await this.tcfClient.sendCommand(new GetChildrenRegistersCommand(parentId))) || [];
	}

	private normalizeRegisterGroupName(v: unknown): string {
		return (typeof v === 'string' ? v : '').trim().toLowerCase();
	}

	/**
	 * Filters the top-level register groups to only those whose group/peripheral name is "CORE".
	 *
	 * NOTE: Some agents expose a single container node at the top; in that case we peek one level
	 * deeper and try to return only its CORE children.
	 */
	private async filterRegisterRootIdsToCoreName(threadContextId: string, rootIds: string[]): Promise<string[]> {
		if (rootIds.length === 0) {
			return rootIds;
		}

		const wanted = new Set(['core', 'core registers']);

		const describe = async (id: string): Promise<{ id: string; name: string }> => {
			try {
				const ctx = await this.tcfClient.sendCommand(new GetContextRegistersCommand(id));
				return { id, name: this.normalizeRegisterGroupName(ctx?.Name) };
			} catch {
				return { id, name: '' };
			}
		};

		const entries = await Promise.all(rootIds.map(describe));
		if (this.startupDiagnosticsEnabled) {
			const preview = entries
				.slice(0, 20)
				.map(e => `${e.id}:${e.name || '(no-name)'}`)
				.join(', ');
		}
		const directMatches = entries.filter(e => wanted.has(e.name) || e.name === 'core');
		if (directMatches.length > 0) {
			return directMatches.map(e => e.id);
		}

		// If we only got a container group at the top, try its children.
		if (entries.length === 1) {
			try {
				const childIds = await this.getRegisterContextChildren(entries[0].id);
				if (childIds.length > 0) {
					const childEntries = await Promise.all(childIds.map(describe));
					if (this.startupDiagnosticsEnabled) {
						const childPreview = childEntries
							.slice(0, 20)
							.map(e => `${e.id}:${e.name || '(no-name)'}`)
							.join(', ');
					}
					const childMatches = childEntries.filter(e => wanted.has(e.name) || e.name === 'core');
					if (childMatches.length > 0) {
						return childMatches.map(e => e.id);
					}
				}
			} catch {
				// best-effort
			}
		}

		// No CORE group detected. Keep existing behavior (show everything), but log when diagnostics are enabled.
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger?.log?.(
				`[dap] filterRegisterRootIdsToCoreName: no CORE group found for ctx=${threadContextId}; leaving unfiltered`
			);
		}
		return rootIds;
	}

	private async registersContextToVariable(registerContextId: string, parentRef: number): Promise<any> {
		const ctx = await this.tcfClient.sendCommand(new GetContextRegistersCommand(registerContextId));
		const name = ctx?.Name || registerContextId;

		const childrenIds = await this.getRegisterContextChildren(registerContextId);
		if (childrenIds.length > 0) {
			return {
				name: () => name,
				displayValue: async () => '',
				getChildren: async () => {
					const children = [];
					for (const childId of childrenIds) {
						try {
							children.push(await this.registersContextToVariable(childId, parentRef));
						} catch (e) {
							// One unreadable register must not hide the whole group.
							const childName = childId.split('.').pop() || childId;
							const message = AurixTCFDebugSession.formatError(e);
							children.push({
								name: () => childName,
								displayValue: async () => `<${message}>`,
								getChildren: async () => [],
							});
						}
					}
					return children;
				},
			};
		}

		if (ctx?.Readable && typeof ctx.Size === 'number' && ctx.Size > 0) {
			const size = ctx.Size;
			const bigEndian = (ctx as any).BigEndian;
			const format = (buf: Buffer): string => {
				// Agents that declare the byte order (BigEndian: false for TriCore) get a
				// numeric value. Otherwise show raw bytes, since endianness varies by target.
				if (bigEndian === false && buf.length > 0 && buf.length <= 8) {
					let hex = '';
					for (let i = buf.length - 1; i >= 0; i--) {
						hex += buf[i].toString(16).padStart(2, '0');
					}
					return `0x${hex}`;
				}
				const hex = buf.toString('hex');
				return hex.length ? `0x${hex}` : '';
			};
			let value = format(await this.tcfClient.sendCommand(new GetRegistersCommand(registerContextId)));
			const writeable = (ctx as any).Writeable === true;
			return {
				name: () => name,
				displayValue: async () => value,
				getChildren: async () => [],
				setValue: writeable
					? async (text: string) => {
						const buf = AurixTCFDebugSession.parseRegisterValue(text, size, bigEndian === true);
						await this.tcfClient.sendCommand(new SetRegistersCommand(registerContextId, buf));
						value = format(await this.tcfClient.sendCommand(new GetRegistersCommand(registerContextId)));
						return value;
					}
					: undefined,
			};
		}

		return {
			name: () => name,
			displayValue: async () => '',
			getChildren: async () => [],
		};
	}

	/**
	 * Parses a register value typed in the Variables view: hex (0x...), binary (0b...)
	 * or decimal, negative values as two's complement of the register size.
	 */
	private static parseRegisterValue(text: string, size: number, bigEndian: boolean): Buffer {
		const t = text.trim().replace(/_/g, '');
		let n: number;
		if (/^-?0x[0-9a-f]+$/i.test(t)) {
			n = Number.parseInt(t.replace(/0x/i, ''), 16);
		} else if (/^0b[01]+$/i.test(t)) {
			n = Number.parseInt(t.substring(2), 2);
		} else if (/^-?[0-9]+$/.test(t)) {
			n = Number.parseInt(t, 10);
		} else {
			throw new Error(`Invalid register value '${text}' (use decimal, 0x hex or 0b binary)`);
		}
		const bytes = Math.min(size, 6);
		if (!Number.isSafeInteger(n) || Math.abs(n) >= 2 ** (8 * bytes)) {
			throw new Error(`Value '${text}' does not fit into ${size} byte(s)`);
		}
		let u = n < 0 ? 2 ** (8 * bytes) + n : n;
		const buf = Buffer.alloc(size);
		for (let i = 0; i < bytes; i++) {
			buf[i] = u % 256;
			u = Math.floor(u / 256);
		}
		if (bigEndian) {
			buf.reverse();
		}
		return buf;
	}

	private static formatError(e: unknown): string {
		if (e instanceof Error) {
			return e.message;
		}
		if (Array.isArray(e)) {
			return e.map(x => AurixTCFDebugSession.formatError(x)).join('; ');
		}
		if (e && typeof e === 'object' && typeof (e as any).Format === 'string') {
			return (e as any).Format;
		}
		return typeof e === 'object' ? JSON.stringify(e) : String(e);
	}

	/** Images to flash: the "flashFiles" launch option, or the program if it is not set. */
	private static getFlashFiles(args: any): string[] {
		const files = Array.isArray(args?.flashFiles)
			? args.flashFiles.filter((f: unknown): f is string => typeof f === 'string' && f.trim() !== '')
			: [];
		if (files.length > 0) {
			return files;
		}
		return typeof args?.program === 'string' && args.program ? [args.program] : [];
	}

	/**
	 * The Tcl command that flashes the images: all files are merged by the server (later files win)
	 * and written in one go, so files sharing a 16 KB sector do not erase each other's data.
	 * - "incremental": sectors that already hold the image are not erased and rewritten.
	 * - "verify" (launch option "verifyFlash", default true): reads the programmed sectors back afterwards.
	 */
	private static flashCommand(files: string[], args: any): string {
		const verify = args?.verifyFlash === false ? '' : ' verify';
		return `flash write_images erase incremental${verify} ${files.map(f => `{${f.replace(/\\/g, '/')}}`).join(' ')}`;
	}

	/** Runs the Tcl commands, printing each with its output and duration, and a total at the end. */
	private async runFlashSteps(steps: string[]): Promise<void> {
		const oldTimeout = (this.tcfClient as any).commandTimeout;
		try {
			this.tcfClient.setCommandTimeout(15 * 60 * 1000);
			const t0 = Date.now();
			for (const step of steps) {
				this.sendEvent(new OutputEvent(`> ${step}\n`, 'console'));
				const t = Date.now();
				const out = await this.tcfClient.sendCommand(new TclEvalCommand(step));
				this.sendEvent(new OutputEvent(`${out ? `${out}\n` : ''}(${((Date.now() - t) / 1000).toFixed(1)} s)\n`, 'console'));
			}
			this.sendEvent(new OutputEvent(`aurix-dbg: flashing done in ${((Date.now() - t0) / 1000).toFixed(1)} s\n`, 'console'));
		} catch (e) {
			this.sendEvent(new OutputEvent(`${AurixTCFDebugSession.formatError(e)}\n`, 'stderr'));
			throw e;
		} finally {
			if (typeof oldTimeout === 'number') {
				this.tcfClient.setCommandTimeout(oldTimeout);
			}
		}
	}

	/**
	 * Flashes the images at launch (enableFlashing), then resets the target into the software.
	 * "reset init" first: PFLASH can only be erased while the cores are halted right after a
	 * reset; once the software has run, the DMU rejects erases with a protection error.
	 */
	private async flashWithAurixDbg(files: string[], args: any): Promise<void> {
		this.sendEvent(new OutputEvent(`aurix-dbg: flashing ${files.join(', ')}\n`, 'console'));
		await this.runFlashSteps(['reset init', AurixTCFDebugSession.flashCommand(files, args), 'reset run']);
	}

	/**
	 * Flashes the images during a debug session (Flash Target toolbar button). The caller resets
	 * the target afterwards like the Reset button does.
	 */
	private async flashInSession(files: string[]): Promise<void> {
		this.sendEvent(new OutputEvent(`aurix-dbg: flashing ${files.join(', ')}\n`, 'console'));
		this.beginResetTransition();
		await this.runFlashSteps(['reset init', AurixTCFDebugSession.flashCommand(files, this.launchArgs)]);
		this.elfFunctionSymbols = undefined;
		// A rebuilt ELF has a new modification time: MemoryMap.set makes the server reload its symbols.
		if (this.launchArgs) {
			await this.initMemoryMap(this.launchArgs);
		}
	}

	protected override initializeRequestAsync(response: DebugProtocol.InitializeResponse, args: DebugProtocol.InitializeRequestArguments): Promise<void> {
		response.body = response.body || {};
		response.body.supportsConfigurationDoneRequest = true;
		response.body.supportsEvaluateForHovers = true;
		(response.body as any).supportsInvalidatedEvent = true;
		response.body.supportsSetVariable = true;
		response.body.supportsSetExpression = true;
		response.body.supportSuspendDebuggee = true;
		response.body.supportTerminateDebuggee = true;
		response.body.supportsInstructionBreakpoints = true;
		response.body.supportsReadMemoryRequest = true;
		response.body.supportsWriteMemoryRequest = true;
		(response.body as any).supportsDisassembleRequest = true;
		this.sendResponse(response);
		return Promise.resolve();
	}


	private parseMemoryReference(ref: string): number | undefined {
		const s = ref.trim();
		if (s === '') {
			return undefined;
		}
		if (/^0x[0-9a-f]+$/i.test(s)) {
			const n = Number.parseInt(s.substring(2), 16);
			return Number.isFinite(n) ? n : undefined;
		}
		if (/^[0-9]+$/.test(s)) {
			const n = Number.parseInt(s, 10);
			return Number.isFinite(n) ? n : undefined;
		}
		const n = Number(s);
		return Number.isFinite(n) ? n : undefined;
	}

	private sumErrorBytes(errors?: MemoryErrorAddress[] | null): number {
		if (!errors || errors.length === 0) {
			return 0;
		}
		return errors.reduce((sum, e) => sum + (Number.isFinite(e?.size) ? e.size : 0), 0);
	}

	private computeWriteWordSize(address: number, length: number): number {
		if (!Number.isFinite(address) || !Number.isFinite(length) || length <= 0) {
			return 1;
		}
		if (address % 4 === 0 && length % 4 === 0) {
			return 4;
		}
		if (address % 2 === 0 && length % 2 === 0) {
			return 2;
		}
		return 1;
	}

	private isPeripheralAddress(address: number): boolean {
		if (!Number.isFinite(address)) {
			return false;
		}
		// aurix-dbg accesses SFRs over TAS without problems: no special handling.
		if (this.agentFlavor === 'aurix-dbg') {
			return false;
		}
		// AURIX peripheral/SFR space lives in the 0xF000_0000+ region.
		return (address >>> 0) >= 0xF0000000;
	}

	private async readMemoryBestEffort(
		// Read memory in chunks to maximize the amount of data read even if some regions are unreadable.
		// For example, read 1 KiB in 64-byte chunks.
		// Returns null if the request is out of bounds for fallback reading.
		// Some memory regions cannot be read at all (e.g. peripheral registers).
		ctx: string,
		address: number,
		count: number
	): Promise<{ base64: string; unreadableBytes?: number } | null> {
		if (count < MEMORY_READ_FALLBACK_MIN_BYTES || count > MEMORY_READ_FALLBACK_MAX_BYTES) {
			return null;
		}

		const buffer = Buffer.alloc(count);
		let unreadableBytes = 0;

		for (let offset = 0; offset < count; offset += MEMORY_READ_FALLBACK_CHUNK_BYTES) {
			const chunkSize = Math.min(MEMORY_READ_FALLBACK_CHUNK_BYTES, count - offset);
			try {
				const result = await this.tcfClient.sendCommand(new GetMemoryCommand(ctx, address + offset, 1, chunkSize, 0));
				const data = result?.base64 ? Buffer.from(result.base64, 'base64') : Buffer.alloc(0);
				if (data.length > 0) {
					data.copy(buffer, offset, 0, Math.min(data.length, chunkSize));
				}
				const missing = Math.max(0, chunkSize - data.length);
				const errorBytes = this.sumErrorBytes(result?.errorAddresses);
				const chunkUnreadable = Math.min(chunkSize, Math.max(missing, errorBytes));
				unreadableBytes += chunkUnreadable;
			} catch {
				unreadableBytes += chunkSize;
			}
		}

		return {
			base64: buffer.toString('base64'),
			unreadableBytes: unreadableBytes > 0 ? unreadableBytes : undefined,
		};
	}

	protected override async readMemoryRequestAsync(response: DebugProtocol.ReadMemoryResponse, args: DebugProtocol.ReadMemoryArguments): Promise<void> {
		// 1) Parse memoryReference + offset from DAP request and compute start address.
		// 2) Pick a suitable context ID (core) to issue the read memory command to.
		// 3) Send GetMemory command to TCF agent.
		// 4) Format and return the read bytes.
		// For example, memoryReference="0x20000000", offset=16, count=32
		// -> read 32 bytes starting at address 0x20000010
		try {
			const memoryReference = typeof args?.memoryReference === 'string' ? args.memoryReference : '';
			const base = this.parseMemoryReference(memoryReference);
			if (base === undefined) {
				this.sendErrorResponse(response, 0, 'Invalid memoryReference');
				return;
			}

			const offset = typeof args?.offset === 'number' && Number.isFinite(args.offset) ? args.offset : 0;
			const count = typeof args?.count === 'number' && Number.isFinite(args.count) ? args.count : 0;
			if (count <= 0) {
				response.body = { address: `0x${(base + offset).toString(16)}`, data: '' };
				this.sendResponse(response);
				return;
			}

			const address = base + offset;
			let ctx = (await this.pickContextIdForCore(this.launchCore)) ?? (await this.pickAnySuspendedContextId());
			if (!ctx) {
				this.sendErrorResponse(response, 0, 'No suspended context for memory read');
				return;
			}
			// AURIX DAS agents crash (ECONNRESET) when peripheral/SFR space (0xF000_0000+) is
			// accessed via Memory.get — regardless of whether the core is suspended or running.
			// ACS (AURIX Configuration Studio) never issues Memory.get on these addresses; it reads
			// peripherals exclusively through the TCF Registers service.
			// Block all peripheral Memory.get to prevent the crash. The peripheral-viewer will show
			// "unreadable" until a Registers-service-based path is implemented.
			if (this.isPeripheralAddress(address)) {
				response.body = {
					address: `0x${address.toString(16)}`,
					data: Buffer.alloc(count).toString('base64'),
					unreadableBytes: count,
				};
				this.sendResponse(response);
				return;
			}
			try {
				const result = await this.tcfClient.sendCommand(new GetMemoryCommand(ctx, address, 1, count, 0));
				const unreadableBytes = this.sumErrorBytes(result?.errorAddresses);
				response.body = {
					address: `0x${address.toString(16)}`,
					data: result?.base64 || '',
					unreadableBytes: unreadableBytes > 0 ? unreadableBytes : undefined,
				};
				this.sendResponse(response);
			} catch (e) {
				const fallback = await this.readMemoryBestEffort(ctx, address, count);
				if (fallback) {
					response.body = {
						address: `0x${address.toString(16)}`,
						data: fallback.base64,
						unreadableBytes: fallback.unreadableBytes,
					};
					this.sendResponse(response);
					return;
				}
				this.sendErrorResponse(response, 0, 'readMemory failed {message}', { message: String(e) });
			}
		} catch (e) {
			this.sendErrorResponse(response, 0, 'readMemory failed {message}', { message: String(e) });
		}
	}

	protected override async writeMemoryRequestAsync(response: DebugProtocol.WriteMemoryResponse, args: DebugProtocol.WriteMemoryArguments): Promise<void> {
		// 1) Parse memoryReference + offset from DAP request and compute start address.
		// 2) Pick a suitable context ID (core) to issue the write memory command to.
		// 3) Send SetMemory command to TCF agent.
		// 4) Format and return the number of bytes written.
		try {
			const memoryReference = typeof args?.memoryReference === 'string' ? args.memoryReference : '';
			const base = this.parseMemoryReference(memoryReference);
			if (base === undefined) {
				this.sendErrorResponse(response, 0, 'Invalid memoryReference');
				return;
			}

			const offset = typeof args?.offset === 'number' && Number.isFinite(args.offset) ? args.offset : 0;
			const dataBase64 = typeof args?.data === 'string' ? args.data : '';
			const bytes = Buffer.from(dataBase64, 'base64');
			const requestedLength = bytes.length;
			const address = base + offset;
			let targetAddress = address;
			let writeBytes = bytes;
			let wordSize = this.computeWriteWordSize(address, bytes.length);
			let writeMode = bytes.length >= 4 && bytes.length % 4 === 0 ? 1 : 0;

			const ctx = (await this.pickContextIdForCore(this.launchCore)) ?? (await this.pickAnySuspendedContextId());
			if (!ctx) {
				this.sendErrorResponse(response, 0, 'No suspended context for memory write');
				return;
			}

			// Peripheral registers often require 32-bit aligned writes.
			// If the request is smaller than 4 bytes, attempt a read-modify-write on the aligned word.
			if (requestedLength > 0 && requestedLength < 4 && this.isPeripheralAddress(address)) {
				const aligned = address - (address % 4);
				const byteOffset = address - aligned;
				try {
					const result = await this.tcfClient.sendCommand(new GetMemoryCommand(ctx, aligned, 1, 4, 0));
					const existing = result?.base64 ? Buffer.from(result.base64, 'base64') : Buffer.alloc(0);
					if (existing.length >= 4) {
						const patched = Buffer.from(existing.subarray(0, 4));
						bytes.copy(patched, byteOffset, 0, Math.min(requestedLength, 4 - byteOffset));
						targetAddress = aligned;
						writeBytes = patched;
						wordSize = 4;
						writeMode = 1;
					}
				} catch {
					// If RMW fails, fall back to the original write request.
				}
			}

			const errors = await this.tcfClient.sendCommand(
				new SetMemoryCommand(ctx, targetAddress, wordSize, writeBytes.length, writeBytes.toString('base64'), writeMode)
			);
			const errorBytes = this.sumErrorBytes(errors);
			if (errorBytes > 0 && args?.allowPartial !== true) {
				const first = errors?.[0];
				const reason = first?.msg ? AurixTCFDebugSession.formatError(first.msg) : 'refused by adapter';
				const at = first && Number.isFinite(first.addr) ? ` at 0x${first.addr.toString(16)}` : '';
				this.sendErrorResponse(response, 0, 'writeMemory failed{at}: {message}', { at, message: reason });
				return;
			}

			response.body = { bytesWritten: Math.max(0, requestedLength - errorBytes) };
			this.sendResponse(response);
		} catch (e) {
			this.sendErrorResponse(response, 0, 'writeMemory failed: {message}', { message: AurixTCFDebugSession.formatError(e) });
		}
	}

	protected override async disassembleRequestAsync(response: any, args: any): Promise<void> {
		// 1) Parse memoryReference + offset from DAP request and compute start address.
		//    (typically around the current PC reported by stackTrace)
		// 2) Pick a suitable context ID (core) to issue the disassemble command to.
		// 3) Query disassembly capabilities (ISAs) if not already cached.
		// 4) Send Disassemble command to TCF agent.
		// 5) Format and return the disassembly lines.
		try {
			const memoryReference = typeof args?.memoryReference === 'string' ? args.memoryReference.trim() : '';
			if (memoryReference === '') {
				response.body = { instructions: [] };
				this.sendResponse(response);
				return;
			}

			const parseAddr = (ref: string): number | undefined => {
				const s = ref.trim();
				if (s === '') {
					return undefined;
				}
				if (/^0x[0-9a-f]+$/i.test(s)) {
					const n = Number.parseInt(s.substring(2), 16);
					return Number.isFinite(n) ? n : undefined;
				}
				if (/^[0-9]+$/.test(s)) {
					const n = Number.parseInt(s, 10);
					return Number.isFinite(n) ? n : undefined;
				}
				const n = Number(s);
				return Number.isFinite(n) ? n : undefined;
			};

			const base = parseAddr(memoryReference);
			if (base === undefined) {
				response.body = { instructions: [] };
				this.sendResponse(response);
				return;
			}

			const offset = typeof args?.offset === 'number' && Number.isFinite(args.offset) ? args.offset : 0;
			const instructionOffset = typeof args?.instructionOffset === 'number' && Number.isFinite(args.instructionOffset)
				? args.instructionOffset
				: 0;
			const instructionCount = typeof args?.instructionCount === 'number' && args.instructionCount > 0 ? args.instructionCount : 32;
			const baseAddress = base + offset;
			// TriCore instructions are 2 or 4 bytes. Use max size to ensure we cover the requested window.
			const minInstrBytes = 2;
			const maxInstrBytes = 4;
			const alignedBaseAddress = baseAddress - (baseAddress % minInstrBytes);
			let startAddress = alignedBaseAddress + (instructionOffset * maxInstrBytes);

			const ctx = (await this.pickContextIdForCore(this.launchCore)) ?? (await this.pickAnySuspendedContextId());
			if (!ctx) {
				response.body = { instructions: [] };
				this.sendResponse(response);
				return;
			}

			if (this.disassemblyCapabilities === undefined) {
				try {
					const caps = await this.tcfClient.sendCommand(new GetCapabilitiesDisassemblyCommand(ctx));
					this.disassemblyCapabilities = (caps || [])
						.map(c => (c?.ISA ? String(c.ISA) : ''))
						.filter(x => x.trim() !== '');
				} catch {
					this.disassemblyCapabilities = [];
				}
			}
			if (!this.disassemblyCapabilitiesLogged) {
				this.disassemblyCapabilitiesLogged = true;
				const capsText = (this.disassemblyCapabilities || []).join(', ');
				this.tcfLogger.log(`Disassembly capabilities (ISAs): ${capsText || '(none reported)'}`);
			}

			// Request enough bytes to produce the requested window + a small margin.
			// The agent disassembler falls back to .byte/.half/.word if ISA-specific decoding is unavailable.
			const windowInstrs = instructionCount + Math.abs(instructionOffset) + 16;
			let byteBudget = Math.max(1, windowInstrs * maxInstrBytes);

			// Clip to executable ranges from the ELF (if available).
			const execRange = this.findExecutableRangeForAddress(alignedBaseAddress);
			if (this.execRanges.length > 0 && !execRange) {
				response.body = { instructions: [] };
				this.sendResponse(response);
				return;
			}
			if (execRange) {
				const maxBytesNeeded = instructionCount * maxInstrBytes;
				let maxStart = execRange.end - maxBytesNeeded;
				if (maxStart < execRange.start) {
					maxStart = execRange.start;
				}
				startAddress = Math.min(Math.max(startAddress, execRange.start), maxStart);
				startAddress = startAddress - (startAddress % minInstrBytes);
				byteBudget = Math.max(1, Math.min(byteBudget, execRange.end - startAddress));
				if (byteBudget <= 0) {
					response.body = { instructions: [] };
					this.sendResponse(response);
					return;
				}
			}
			const disassembleAt = async (addr: number) => {
				return await this.tcfClient.sendCommand(
					new DisassembleDisassemblyCommand(ctx, addr, byteBudget, {
						ISA: 'TRICORE',
						Simplified: true,
						PseudoInstructions: true,
						OpcodeValue: true,
					} as any)
				);
			};
			let lines = await disassembleAt(startAddress);

			const toHexBytes = (opcodeValue: any): string | undefined => {
				if (typeof opcodeValue !== 'string' || opcodeValue.trim() === '') {
					return undefined;
				}
				try {
					const buf = Buffer.from(opcodeValue, 'base64');
					if (buf.length === 0) {
						return undefined;
					}
					return Array.from(buf.values()).map(b => b.toString(16).padStart(2, '0')).join(' ');
				} catch {
					return undefined;
				}
			};

			const toInstruction = (l: any, fallbackAddr: number): DebugProtocol.DisassembledInstruction => {
				const addr = typeof l?.Address === 'number' ? l.Address : fallbackAddr;
				const parts = Array.isArray(l?.Instruction) ? l.Instruction : [];
				const text = parts.map((p: any) => (p?.Text ? String(p.Text) : '')).join('');
				return {
					address: `0x${addr.toString(16)}`,
					instruction: this.formatDisassemblyText(text, toHexBytes(l?.OpcodeValue)),
				};
			};

			let list = Array.isArray(lines) ? lines : [];
			let instructions = list.map((l: any) => toInstruction(l, startAddress));

			// Align the returned window to match instructionOffset semantics.
			let baseIndex = instructions.findIndex((i) => parseAddr(i.address) === alignedBaseAddress);
			if (baseIndex < 0) {
				// Retry disassembly anchored at the aligned base address.
				lines = await disassembleAt(alignedBaseAddress);
				list = Array.isArray(lines) ? lines : [];
				instructions = list.map((l: any) => toInstruction(l, alignedBaseAddress));
				baseIndex = instructions.findIndex((i) => parseAddr(i.address) === alignedBaseAddress);
			}
			let startIndex = 0;
			if (baseIndex >= 0) {
				startIndex = baseIndex + instructionOffset;
			}
			if (startIndex < 0) {
				startIndex = 0;
			}

			instructions = instructions.slice(startIndex, startIndex + instructionCount);

			response.body = { instructions: await this.withDisassemblySource(ctx, instructions) };
			this.sendResponse(response);
		} catch (e) {
			this.sendErrorResponse(response, 0, 'Disassemble error {message}', { message: String(e) });
		}
	}

	/**
	 * Puts the opcode bytes into the instruction text instead of `instructionBytes`: VS Code pads that
	 * column to 30 characters, while a TriCore instruction has at most 4 bytes ("xx xx xx xx").
	 */
	private formatDisassemblyText(text: string, bytes: string | undefined): string {
		if (!bytes || this.disassemblyOpcodeBytes === 'hidden') {
			return text;
		}
		if (this.disassemblyOpcodeBytes === 'right') {
			// Expand tabs so the padding lines the bytes up in a column.
			let expanded = '';
			for (const ch of text) {
				expanded = ch === '\t' ? expanded.padEnd((Math.floor(expanded.length / 8) + 1) * 8) : expanded + ch;
			}
			return `${expanded.padEnd(DISASSEMBLY_INSTRUCTION_WIDTH)}${DISASSEMBLY_COLUMN_GAP}${bytes}`;
		}
		return `${bytes.padEnd(DISASSEMBLY_BYTES_WIDTH)}${DISASSEMBLY_COLUMN_GAP}${text}`;
	}

	/**
	 * Adds the source location to disassembled instructions, which VS Code shows between the
	 * instructions when "Debug › Disassembly View: Show Source Code" is enabled.
	 */
	private async withDisassemblySource(
		ctx: string,
		instructions: Array<{ address: string; instruction: string; instructionBytes?: string }>
	): Promise<DebugProtocol.DisassembledInstruction[]> {
		const addrs = instructions.map(i => Number.parseInt(i.address.substring(2), 16));
		if (addrs.length === 0) {
			return instructions;
		}
		let areas: TCFCodeAreaLineNumbers[] = [];
		try {
			// One query for the whole window: the agent returns every line entry overlapping it.
			areas = (await this.tcfClient.sendCommand(
				new MapToSourceLineNumbersCommand(ctx, Math.min(...addrs), Math.max(...addrs) + 4)
			)) || [];
		} catch (e) {
			this.tcfLogger.error(`Disassembly source lookup failed: ${String(e)}`);
			return instructions;
		}
		const sources = new Map<string, Source | undefined>();
		return instructions.map((ins, idx) => {
			const addr = addrs[idx];
			const area = areas.find(a => a.File && a.SLine && a.SAddr !== undefined && a.EAddr !== undefined && addr >= a.SAddr && addr < a.EAddr);
			if (!area) {
				return ins;
			}
			const key = `${area.Dir ?? ''}|${area.File}`;
			if (!sources.has(key)) {
				const sourcePath = this.resolveMappedSourcePath(area);
				sources.set(key, sourcePath ? new Source(path.basename(sourcePath), sourcePath) : undefined);
			}
			const location = sources.get(key);
			if (!location) {
				return ins;
			}
			return { ...ins, location, line: area.SLine, endLine: area.ELine ?? area.SLine };
		});
	}


	protected override async launchRequestAsync(response: DebugProtocol.LaunchResponse, rawArgs: DebugProtocol.LaunchRequestArguments): Promise<void> {
		const args = rawArgs as AurixTCFLaunchRequestArguments;
		this.launchArgs = args;
		this.launchCore = args.core;
		this.agentFlavor = args.agentFlavor === 'aurix-dbg' ? 'aurix-dbg' : 'infineon';
		this.instructionSteppingEnabled = args.instructionStepping === true;
		this.liveWatchEnabled = args.liveWatchEnabled === true;
		this.liveWatchIntervalMs = this.normalizeLiveWatchIntervalMs(args.liveWatchIntervalMs);
		this.launchResetType = typeof args.resetType === 'string' && args.resetType.trim() !== '' ? args.resetType.trim() : undefined;
		// Default: run after reset so we stop at first breakpoint / stop-at-function.
		this.launchResetSuspend = args.resetSuspend === true;
		const host = args.host || DEFAULT_TCF_HOST;
		const port = args.port || DEFAULT_TCF_PORT;
		this.breakpointPrefix = args.breakpointPrefix ?? DEFAULT_BREAKPOINTS_PREFIX;
		this.programPath = args.program;
		if (this.programPath && this.programPath.trim() !== '') {
			try {
				this.execRanges = this.loadExecutableRangesFromElf(this.programPath);
				if (this.execRanges.length > 0) {
					this.tcfLogger.log(
						`ELF executable ranges: ${this.execRanges.map(r => `0x${r.start.toString(16)}-0x${r.end.toString(16)}`).join(', ')}`
					);
				}
			} catch (e) {
				this.execRanges = [];
				this.tcfLogger.error(`Failed to parse ELF executable ranges: ${String(e)}`);
			}
		}
		this.workspaceBaseDir = undefined;
		this.projectRootDir = undefined;
		if (args.workspace && args.workspace.trim() !== '') {
			// Best-effort: args.workspace may be a folder or a workspace-file path.
			const wsPath = path.resolve(args.workspace);
			try {
				const st = fs.statSync(wsPath);
				this.workspaceBaseDir = st.isDirectory() ? wsPath : path.dirname(wsPath);
			} catch {
				this.workspaceBaseDir = path.dirname(wsPath);
			}
		}
		this.configuredGlobals = Array.isArray(args.globals) ? args.globals.filter(x => typeof x === 'string' && x.trim() !== '') : [];
		await this.initDefaultPathMapper(args);

		// Detect the local project root (where "Libraries" lives). This is used to remap
		// non-local absolute paths reported by the toolchain (e.g. a different workspace root).
		const rootSearchStarts = [this.workspaceBaseDir, this.sourceResolverBaseDir].filter(Boolean) as string[];
		for (const start of rootSearchStarts) {
			let dir = start;
			for (let i = 0; i < 10; i++) {
				try {
					if (fs.existsSync(path.join(dir, 'Libraries'))) {
						this.projectRootDir = dir;
						break;
					}
				} catch {
					// ignore
				}
				const parent = path.dirname(dir);
				if (parent === dir) {
					break;
				}
				dir = parent;
			}
			if (this.projectRootDir) {
				break;
			}
		}

		// No local DWARF tooling: rely on agent-provided line info (mapToSource) + MemoryMap.

		if (typeof args.stackTraceDepth === 'number' && args.stackTraceDepth > 0) {
			this.stackTraceDepth = args.stackTraceDepth;
		}

		this.tcfLogger.setDebugTCFMessages(Boolean(args.debugTCFMessages || args.internal?.debugTCFMessages));
		this.uiFocusDiagnosticsEnabled = Boolean(args.debugUIFocus || args.internal?.debugUIFocus);
		if (args.internal?.commandToken) {
			(this.tcfClient as any).setCommandToken(args.internal.commandToken);
		}

		if (args.pathMapper && args.pathMapper.trim() !== '') {
			if (this.enabledPathMapper) {
				try {
					const factory = new Function(args.pathMapper);
					const mapper = factory();
					if (typeof mapper === 'function') {
						this.pathMapper = mapper;
					}
				} catch (e) {
					this.tcfLogger.error(`Failed to load pathMapper: ${String(e)}`);
				}
			} else {
				this.tcfLogger.error('Ignoring pathMapper (not enabled / untrusted workspace).');
			}
		}

		if (typeof args.timeout === 'number') {
			this.tcfClient.setCommandTimeout(args.timeout);
		}
		this.tcfLogger.setPrefix('');

		const stopOnEntry = Boolean(args.stopOnEntry);
		if (typeof args.stopStormPauseDelayMs === 'number' && Number.isFinite(args.stopStormPauseDelayMs) && args.stopStormPauseDelayMs >= 0) {
			this.stopStormPauseDelayMs = args.stopStormPauseDelayMs;
		}
		if (typeof args.refocusOnWrongContext === 'boolean') {
			this.refocusOnWrongContext = args.refocusOnWrongContext;
		}
		this.startupDiagnosticsEnabled =
			stopOnEntry ||
			Boolean(args.debugTCFMessages || args.internal?.debugTCFMessages || (args.internal as any)?.debugStartup) ||
			this.uiFocusDiagnosticsEnabled;
		this.startupLog(
			`launch host=${host}:${port} core=${String(args.core)} stopOnEntry=${String(args.stopOnEntry)}(${typeof args.stopOnEntry}) resumeOtherCoresOnEntry=${String(args.resumeOtherCoresOnEntry)} entryFunction=${String(args.entryFunction ?? '')} resetType=${String(this.launchResetType ?? '')} resetSuspend=${String(this.launchResetSuspend)}`
		);
		this.startupLog(`stopStormPauseDelayMs=${this.stopStormPauseDelayMs}`);
		this.startupLog(`refocusOnWrongContext=${String(this.refocusOnWrongContext)}`);
		this.startupLog(`stopStormStackSuppressMs=${this.stopStormStackSuppressMs} (fixed)`);

		try {
			this.startupLog('connecting...');
			await this.tcfClient.connect(host, port);
			this.startupLog('connected');
			const flashFiles = AurixTCFDebugSession.getFlashFiles(args);
			if (this.agentFlavor === 'aurix-dbg' && args.enableFlashing === true && flashFiles.length > 0) {
				try {
					await this.flashWithAurixDbg(flashFiles, args);
				} catch (e) {
					this.sendErrorResponse(response, 1, 'Flashing with aurix-dbg failed: {message}', { message: AurixTCFDebugSession.formatError(e) });
					return;
				}
			}
			await this.initMemoryMap(args);
			this.refreshLiveWatchTimer();
			this.sendResponse(response);
			this.sendEvent(new InitializedEvent());
			await this.startupLogThreads('after InitializedEvent');

			// If the target is already suspended (e.g. agent stops on entry by itself),
			// proactively inform VS Code so the toolbar shows Resume/Step.
			try {
				const initialContext = await this.pickContextIdForCore(args.core);
				this.startupLog(`initial suspend check: context=${String(initialContext ?? '')}`);
				if (initialContext) {
					await this.emitStoppedIfSuspended(initialContext, StoppedEventReason.pause);
				}
			} catch {
				// best-effort only
			}
			if (args.entryFunction && args.stopOnEntry) {
				this.tcfLogger.log('stopOnEntry is true while entryFunction is set; will request a pause after launch so VS Code can show Call Stack/Variables.');
			}

			if (stopOnEntry) {
				try {
					await this.startupLogThreads('before stopOnEntry pause');
					const targetContext = await this.pickContextIdForCore(args.core);
					this.startupLog(
						`stopOnEntry: pause request mode=${args.resumeOtherCoresOnEntry === true ? 'single-core' : 'all-cores'} target=${String(targetContext ?? '')}`
					);
					// Default: stop the whole system and then focus the requested core.
					// This matches typical multi-core "stop at function" behavior.
					if (args.resumeOtherCoresOnEntry === true) {
						await this.tcfClient.pause(targetContext);
					} else {
						// Avoid relying on cached contextInfo/top-level selection; explicitly pause each core.
						await this.pauseAllCoresBestEffort('stopOnEntry');
					}
					await this.startupLogThreads('after stopOnEntry pause');
					if (args.resumeOtherCoresOnEntry === true) {
						// Only one core is expected to be stopped.
						if (targetContext) {
							await this.emitStoppedIfSuspended(targetContext, StoppedEventReason.entry);
							await this.resumeOtherCoresIfNeeded(targetContext, args);
						}
					} else {
						// We paused all cores; ensure VS Code marks all threads as stopped.
						await this.emitStoppedForAllSuspendedAtStartup(targetContext);
					}
					await this.startupLogThreads('after emitStopped/resumeOtherCoresIfNeeded');
				} catch (e) {
					this.tcfLogger.error(`stopOnEntry requested but pause failed: ${String(e)}`);
				}
			}
		} catch (e) {
			this.sendErrorResponse(response, 1, `Could not connect to ${host}:${port}. {message}`, { message: String(e) });
		}
	}

	protected override async disconnectRequestAsync(response: DebugProtocol.DisconnectResponse): Promise<void> {
		this.tcfClient.disconnect();
		this.sendResponse(response);
		this.sendEvent(new TerminatedEvent());
	}

	protected override async configurationDoneRequestAsync(response: DebugProtocol.ConfigurationDoneResponse): Promise<void> {
		this.breakpointsManager.setConfigured();
		await this.syncAllBreakpoints();
		this.sendResponse(response);
	}

	protected override async threadsRequestAsync(response: DebugProtocol.ThreadsResponse): Promise<void> {
		const infos = await this.tcfClient.getThreads();
		const threads = infos.map(x => {
			const id = this.getOrAssignThreadId(x.info.ID);
			return new Thread(id, x.info.Name || x.info.ID);
		});
		if (this.startupDiagnosticsEnabled) {
			const list = threads.map(t => `${t.id}:${t.name}`).join(', ');
			this.tcfLogger.log(`[dap] ThreadsResponse threads=${threads.length}${threads.length ? ` ${list}` : ''}`);
		}
		response.body = { threads };
		this.sendResponse(response);
	}

	protected override async stackTraceRequestAsync(response: DebugProtocol.StackTraceResponse, args: DebugProtocol.StackTraceArguments): Promise<void> {
		const contextId = this.threadIdToContext.get(args.threadId);
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(`[dap] StackTraceRequest threadId=${args.threadId} ctx=${String(contextId ?? '')}`);
		}
		const p = this.primaryBreakpointStop;
		if (p && args.threadId !== p.threadId) {
			// Multi-core UX: previously we returned an empty stack for non-primary threads for a short
			// window after a breakpoint stop (to reduce focus thrash during "stop storms").
			// That can leave other cores' call stacks blank until the user manually refreshes.
			// Instead, just delay responding until the suppression window passes.
			const remainingMs = p.stackSuppressUntilMs - Date.now();
			if (remainingMs > 0) {
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(
						`[dap] StackTraceDelayed threadId=${args.threadId} ctx=${String(contextId ?? '')} primaryThreadId=${p.threadId} primaryCtx=${p.ctx} delayMs=${remainingMs}`
					);
				}
				await new Promise(resolve => setTimeout(resolve, remainingMs));
			}
		}
		if (!contextId) {
			response.body = { stackFrames: [], totalFrames: 0 };
			this.sendResponse(response);
			return;
		}

		const stack = this.trimPostResetStack(await this.tcfClient.getStackTrace(contextId, NEVER_CANCELLED, this.stackTraceDepth));
		const frames: StackFrame[] = [];
		for (const frame of stack) {
			const frameId = this.createNextStackFrameId(args.threadId, frame.context);
			this.rememberFrameMeta(frameId, args.threadId, contextId, frame.context?.ID);
			const mapped = frame.mapToSource;
			const ipNum = typeof frame.context?.IP === 'number' ? frame.context.IP : undefined;
			// Align to instruction boundary (TriCore has 2/4-byte instructions).
			const ipAligned = ipNum !== undefined ? (ipNum - (ipNum % 2)) : undefined;
			const ipRef = ipAligned !== undefined ? `0x${ipAligned.toString(16)}` : undefined;
			if (mapped?.File && mapped?.SLine) {
				const sourcePath = this.resolveMappedSourcePath(mapped);
				const src = sourcePath ? new Source(path.basename(sourcePath), sourcePath) : undefined;
				const sf = new StackFrame(frameId, frame.context.Name || frame.context.ID, src, mapped.SLine, 1);
				(sf as any).instructionPointerReference = ipRef;
				frames.push(sf);
				continue;
			}

			const sf = new StackFrame(frameId, frame.context.Name || frame.context.ID);
			(sf as any).instructionPointerReference = ipRef;
			frames.push(sf);
		}

		const jump = this.disassemblyJumpTarget;
		if (jump && stack.length > 0) {
			// Shares the top frame's context so Locals and watches keep working while it is focused.
			const frameId = this.createNextStackFrameId(args.threadId, stack[0].context);
			this.rememberFrameMeta(frameId, args.threadId, contextId, stack[0].context?.ID);
			const sf = new StackFrame(frameId, `[Disassembly] ${jump.label}`);
			(sf as any).instructionPointerReference = `0x${jump.address.toString(16)}`;
			frames.push(sf);
		}

		response.body = { stackFrames: frames, totalFrames: frames.length };
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(`[dap] StackTraceResponse threadId=${args.threadId} ctx=${contextId} frames=${frames.length}`);
		}
		this.sendResponse(response);
	}

	protected override async scopesRequestAsync(response: DebugProtocol.ScopesResponse, args: DebugProtocol.ScopesArguments): Promise<void> {
		if (this.startupDiagnosticsEnabled) {
			const resolved = this.tryResolveFrame(args.frameId);
			this.tcfLogger.log(
				`[dap] ScopesRequest frameId=${args.frameId} threadId=${String(resolved.threadId ?? '')} ctx=${String(resolved.ctx ?? '')}`
			);
			this.maybeRefocusToBreakpointThread('ScopesRequest', resolved);
		}
		try {
			response.body = {
				scopes: [
					new Scope('Locals', this.getLocalScopeVariableReference(args.frameId), false),
					// new Scope('Globals', this.getGlobalScopeVariableReference(args.frameId), false),
					new Scope('CORE Registers', this.getRegisterScopeVariableReference(args.frameId), false),
				]
			};
			this.sendResponse(response);
		} catch (e) {
			const msg = e instanceof Error ? e.message : String(e);
			const isStaleRef = msg.startsWith('Unknown stack frame') || msg.startsWith('Unknown object reference');
			if (isStaleRef) {
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(`[dap] ScopesRequest suppressed: ${msg}`);
				}
				response.body = { scopes: [] };
				this.sendResponse(response);
				return;
			}
			throw e;
		}
	}

	protected override async variablesRequestAsync(response: DebugProtocol.VariablesResponse, args: DebugProtocol.VariablesArguments): Promise<void> {
		if (this.startupDiagnosticsEnabled) {
			let ctx = '';
			try {
				const details = this.getStackFrameDetails(args.variablesReference);
				ctx = details?.[0] ?? '';
			} catch {
				// ignore
			}
			const kind = this.isSubVariableReference(args.variablesReference)
				? 'sub'
				: this.isRegisterScopeVariableReference(args.variablesReference)
					? 'registers'
					: this.isGlobalScopeVariableReference(args.variablesReference)
						? 'globals'
						: this.isLocalScopeVariableReference(args.variablesReference)
							? 'locals'
							: 'unknown';
			const threadId = ctx ? (this.contextToThreadId.get(ctx) ?? this.getOrAssignThreadId(ctx)) : undefined;
			this.tcfLogger.log(
				`[dap] VariablesRequest ref=${args.variablesReference} kind=${kind} threadId=${String(threadId ?? '')} ctx=${String(ctx)}`
			);
			this.maybeRefocusToBreakpointThread('VariablesRequest', { threadId, ctx });
		}
		try {
			if (this.isSubVariableReference(args.variablesReference)) {
				const vars = this.getSubVariable(args.variablesReference);
				const dapVars = await Promise.all(vars.map(v => this.toDAPVariable(v, args.variablesReference)));
				response.body = { variables: dapVars };
				this.sendResponse(response);
				return;
			}

			const [ctx, stack] = this.getStackFrameDetails(args.variablesReference);
			const wantRegisters = this.isRegisterScopeVariableReference(args.variablesReference);
			if (wantRegisters) {
				const rootIds = await this.getRegisterContextChildren(ctx);
				const filteredRootIds = await this.filterRegisterRootIdsToCoreName(ctx, rootIds);
				const regVars = [];
				for (const id of filteredRootIds) {
					regVars.push(await this.registersContextToVariable(id, args.variablesReference));
				}
				const dapVars = await Promise.all(regVars.map(v => this.toDAPVariable(v, args.variablesReference)));
				response.body = { variables: dapVars };
				this.sendResponse(response);
				return;
			}

			const wantGlobals = this.isGlobalScopeVariableReference(args.variablesReference);
			if (wantGlobals) {
				if (this.configuredGlobals.length === 0) {
					response.body = { variables: [] };
					this.sendResponse(response);
					return;
				}

				const globals = [];
				for (const expr of this.configuredGlobals) {
					try {
						// AURIX agent: createInScope can hang; prefer standard Expressions.create.
						const exprId = await this.tcfClient.sendCommand(new CreateExpressionsCommand(ctx, null, expr));
						const v = await this.tcfClient.getExpressionIDVariable(ctx, stack, exprId);
						AurixTCFDebugSession.annotateVariable(v, expr, ctx);
						if (v) {
							globals.push(v);
						}
					} catch (e) {
						this.tcfLogger.error(`Failed to evaluate global '${expr}': ${String(e)}`);
					}
				}
				const dapVars = await Promise.all(globals.map(v => this.toDAPVariable(v, args.variablesReference)));
				response.body = { variables: dapVars };
				this.sendResponse(response);
				return;
			}

			const vars = await this.tcfClient.getStackVariables(ctx, stack);
			for (const v of vars) {
				AurixTCFDebugSession.annotateVariable(v, v.name(), stack);
			}
			const filtered = vars.filter(v => v.isRegister() === false);
			const dapVars = await Promise.all(filtered.map(v => this.toDAPVariable(v, args.variablesReference)));
			response.body = { variables: dapVars };
			this.sendResponse(response);
		} catch (e) {
			const msg = e instanceof Error ? e.message : String(e);
			const isStaleRef =
				msg.startsWith('Unknown object reference') ||
				msg.startsWith('Unknown stack frame') ||
				msg.startsWith('Unknown sub variable reference');
			const isExecutionRunning = (() => {
				const maybe = e as any;
				if (Array.isArray(maybe)) {
					return maybe.some((x) => (x as any)?.Code === 14 || String((x as any)?.Format ?? '').includes('Execution context is running'));
				}
				return maybe?.Code === 14 || String(maybe?.Format ?? '').includes('Execution context is running');
			})();

			// VS Code can issue Variables requests while stepping/resetting, and it can also
			// re-request old references after we invalidated them. These are benign.
			if (isStaleRef || isExecutionRunning) {
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(`[dap] VariablesRequest suppressed: ${msg}`);
				}
				response.body = { variables: [] };
				this.sendResponse(response);
				return;
			}

			this.sendErrorResponse(response, 0, 'Variables error {message}', { message: msg });
		}
	}

	protected override async setVariableRequestAsync(response: DebugProtocol.SetVariableResponse, args: DebugProtocol.SetVariableArguments): Promise<void> {
		try {
			let vars: any[] = [];
			if (this.isSubVariableReference(args.variablesReference)) {
				vars = this.getSubVariable(args.variablesReference);
			} else if (!this.isRegisterScopeVariableReference(args.variablesReference) && !this.isGlobalScopeVariableReference(args.variablesReference)) {
				const [ctx, stack] = this.getStackFrameDetails(args.variablesReference);
				vars = await this.tcfClient.getStackVariables(ctx, stack);
				for (const v of vars) {
					AurixTCFDebugSession.annotateVariable(v, v.name?.(), stack);
				}
			}
			const variable = vars.find(v => (v?.name?.() ?? '') === args.name);
			if (variable && typeof variable.setValue === 'function') {
				const value = await variable.setValue(args.value);
				response.body = { value: String(value ?? (await variable.displayValue?.()) ?? '') };
				this.sendResponse(response);
				return;
			}
			if (variable?.__aurixExpr && variable?.__aurixScope) {
				response.body = { value: await this.assignExpression(variable.__aurixScope, variable.__aurixExpr, args.value) };
				this.sendResponse(response);
				return;
			}
			this.sendErrorResponse(response, 0, `'${args.name}' cannot be modified`);
		} catch (e) {
			this.sendErrorResponse(response, 0, 'Could not set {name}: {message}', { name: args.name, message: AurixTCFDebugSession.formatError(e) });
		}
	}

	protected override async setExpressionRequestAsync(response: DebugProtocol.SetExpressionResponse, args: DebugProtocol.SetExpressionArguments): Promise<void> {
		try {
			let scope: string | undefined;
			if (typeof args.frameId === 'number') {
				scope = this.getStackFrameDetails(this.getLocalScopeVariableReference(args.frameId))[1];
			} else {
				scope = (await this.pickAnySuspendedContextId()) ?? (await this.pickContextIdForRunningWatch());
			}
			if (!scope) {
				this.sendErrorResponse(response, 0, 'No context to evaluate {expression} in', { expression: args.expression });
				return;
			}
			response.body = { value: await this.assignExpression(scope, args.expression.trim(), args.value) };
			this.sendResponse(response);
		} catch (e) {
			this.sendErrorResponse(response, 0, 'Could not set {expression}: {message}', { expression: args.expression, message: AurixTCFDebugSession.formatError(e) });
		}
	}

	/**
	 * Assigns a value typed in the UI to a C expression (Expressions.assign) and returns the new
	 * display value. `scope` is a stack frame or core context.
	 */
	private async assignExpression(scope: string, expression: string, valueText: string): Promise<string> {
		const exprId = await this.tcfClient.sendCommand(new CreateExpressionsCommand(scope, null, expression));
		const exprCtx = await this.tcfClient.sendCommand(new GetContextExpressionsCommand(exprId));
		if (!exprCtx?.CanAssign || !exprCtx.Type || exprCtx.Size === undefined) {
			throw new Error(`'${expression}' is not assignable`);
		}
		const type = await this.tcfClient.sendCommand(new GetContextSymbolsCommand(exprCtx.Type));
		const enumerators = async () => {
			const ids = (await this.tcfClient.sendCommand(new GetChildrenSymbolsCommand(type.ID))) || [];
			const out: Array<{ name: string; value: Buffer }> = [];
			for (const id of ids) {
				const c = await this.tcfClient.sendCommand(new GetContextSymbolsCommand(id));
				if (c.Name && c.Value) {
					out.push({ name: c.Name, value: Buffer.from(c.Value, 'base64') });
				}
			}
			return out;
		};
		const data = await encodeVariableValue(valueText, type, exprCtx.Size, enumerators);
		await this.tcfClient.sendCommand(new AssignExpressionsCommand(exprId, data));
		const core = scope.replace(/\.F\d+$/, '');
		const v = await this.tcfClient.getExpressionIDVariable(core, scope, exprId, expression);
		return (await v?.displayValue?.()) ?? valueText;
	}

	protected override async evaluateRequestAsync(response: DebugProtocol.EvaluateResponse, args: DebugProtocol.EvaluateArguments): Promise<void> {
		if (this.startupDiagnosticsEnabled) {
			if (typeof args.frameId === 'number') {
				const resolved = this.tryResolveFrame(args.frameId);
				this.tcfLogger.log(
					`[dap] EvaluateRequest frameId=${args.frameId} threadId=${String(resolved.threadId ?? '')} ctx=${String(resolved.ctx ?? '')} expr=${String(args.expression ?? '')}`
				);
				this.maybeRefocusToBreakpointThread('EvaluateRequest', resolved);
			} else {
				this.tcfLogger.log(`[dap] EvaluateRequest frameId=(none) expr=${String(args.expression ?? '')}`);
			}
		}
		try {
			if (typeof args.expression !== 'string' || args.expression.trim() === '') {
				response.body = { result: '', variablesReference: 0 };
				this.sendResponse(response);
				return;
			}

			const expressionText = args.expression.trim();

			// "monitor <cmd>" in the debug console: run a Tcl command on the debug server.
			const monitor = /^(?:monitor|mon)\s+([\s\S]+)$/i.exec(expressionText);
			if (args.context === 'repl' && monitor) {
				if (!this.tcfClient.supportedServices?.includes(TCL_SERVICE)) {
					this.sendErrorResponse(response, 0, 'This TCF agent has no Tcl console (monitor commands need aurix-dbg)');
					return;
				}
				try {
					const out = await this.tcfClient.sendCommand(new TclEvalCommand(monitor[1]));
					response.body = { result: out, variablesReference: 0 };
					this.sendResponse(response);
				} catch (e) {
					this.sendErrorResponse(response, 0, '{message}', { message: AurixTCFDebugSession.formatError(e) });
				}
				return;
			}

			const isWatchRequest = args.context === 'watch';
			const liveRefreshable = this.isLiveWatchRefreshableExpression(expressionText);

			let ctx: string | undefined;
			let stack: string | undefined;
			let parentRef: number | undefined;
			let createParentContext: string | undefined;
			if (isWatchRequest && this.liveWatchEnabled && liveRefreshable) {
				ctx = await this.pickContextIdForRunningWatch();
				if (ctx) {
					parentRef = 0;
					createParentContext = ctx;
				}
			}
			if (!createParentContext && typeof args.frameId === 'number') {
				parentRef = this.getLocalScopeVariableReference(args.frameId);
				[ctx, stack] = this.getStackFrameDetails(parentRef);
				createParentContext = stack;
			} else if (!createParentContext) {
				// Watch / REPL sometimes evaluate without a frameId.
				parentRef = 0;
				if (!createParentContext) {
					ctx = await this.pickAnySuspendedContextId();
					if (ctx) {
						const stacks = (await this.tcfClient.sendCommand(new GetChildrenStackTraceCommand(ctx))) || [];
						stack = stacks.length > 0 ? stacks[stacks.length - 1] : undefined;
						createParentContext = stack;
					}
				}
			}

			if (!ctx || !createParentContext) {
				if (this.tryRespondWithCachedWatchValue(response, expressionText, isWatchRequest)) {
					return;
				}
				response.body = { result: '', variablesReference: 0 };
				this.sendResponse(response);
				return;
			}

			// AURIX agent: createInScope can hang; prefer standard Expressions.create.
			const exprId = await this.tcfClient.sendCommand(new CreateExpressionsCommand(createParentContext, null, expressionText));
			const v = await this.tcfClient.getExpressionIDVariable(ctx, stack ?? createParentContext, exprId, expressionText);

			if (!v) {
				if (this.tryRespondWithCachedWatchValue(response, expressionText, isWatchRequest)) {
					return;
				}
				response.body = { result: '', variablesReference: 0 };
				this.sendResponse(response);
				return;
			}
			AurixTCFDebugSession.annotateVariable(v, expressionText, createParentContext);

			let variablesReference = 0;
			try {
				const children = await v.getChildren?.();
				if (children && children.length > 0) {
					AurixTCFDebugSession.annotateChildren(v, children);
					variablesReference = parentRef ? this.createSubVariableReference(parentRef, children) : 0;
				}
			} catch {
				// ignore child errors
			}

			response.body = {
				result: (await v.displayValue?.()) ?? '',
				variablesReference,
			};
			if (isWatchRequest) {
				this.watchExpressionStates.set(expressionText, {
					result: response.body.result,
					liveRefreshable,
				});
				this.refreshLiveWatchTimer();
			}
			this.sendResponse(response);
		} catch (e) {
			const msg = e instanceof Error ? e.message : String(e);
			const isStaleRef = msg.startsWith('Unknown stack frame') || msg.startsWith('Unknown object reference');
			const isExecutionRunning = (() => {
				const maybe = e as any;
				if (Array.isArray(maybe)) {
					return maybe.some((x) => (x as any)?.Code === 14 || String((x as any)?.Format ?? '').includes('Execution context is running'));
				}
				return maybe?.Code === 14 || String(maybe?.Format ?? '').includes('Execution context is running');
			})();

			// VS Code may ask for evaluation during stepping/reset/continue and may reuse old frameIds.
			// Treat these as benign UI refresh artifacts.
			if (isStaleRef || isExecutionRunning) {
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(`[dap] EvaluateRequest suppressed: ${msg}`);
				}
				if (typeof args.expression === 'string' && this.tryRespondWithCachedWatchValue(response, args.expression.trim(), args.context === 'watch')) {
					return;
				}
				response.body = { result: '', variablesReference: 0 };
				this.sendResponse(response);
				return;
			}

			this.sendErrorResponse(response, 0, 'Evaluate error {message}', { message: msg });
		}
	}

	private async pickAnySuspendedContextId(): Promise<string | undefined> {
		const known = Array.from(this.suspendedContextIds.values());
		if (known.length > 0) {
			return known[0];
		}
		try {
			const infos = await this.tcfClient.getThreads();
			const suspended = infos.find(t => Boolean(t.state?.suspended));
			return suspended?.info.ID;
		} catch {
			return undefined;
		}
	}

	private normalizeLiveWatchIntervalMs(value: unknown): number {
		const numeric = typeof value === 'number' ? value : Number(value);
		if (!Number.isFinite(numeric)) {
			return 1000;
		}
		return Math.min(10_000, Math.max(200, Math.round(numeric)));
	}

	private isLiveWatchRefreshableExpression(expressionText: string): boolean {
		return /^[A-Za-z_][A-Za-z0-9_]*(?:(?:\.[A-Za-z_][A-Za-z0-9_]*)|(?:\[[0-9]+\]))*$/.test(expressionText);
	}

	private tryRespondWithCachedWatchValue(response: DebugProtocol.EvaluateResponse, expressionText: string, isWatchRequest: boolean): boolean {
		if (!isWatchRequest) {
			return false;
		}
		const cached = this.watchExpressionStates.get(expressionText);
		if (!cached) {
			return false;
		}
		response.body = { result: cached.result, variablesReference: 0 };
		this.sendResponse(response);
		return true;
	}

	private async pickContextIdForRunningWatch(): Promise<string | undefined> {
		const infos = await this.tcfClient.getThreads();
		const running = infos.filter(t => !t.state?.suspended);
		if (running.length === 0) {
			return undefined;
		}
		if (this.launchCore !== undefined) {
			const idRe = new RegExp(`(?:^|\\.)C${this.launchCore}(?:$|\\b)`, 'i');
			const nameRe = new RegExp(`\\bcore\\s*${this.launchCore}\\b|\\bC${this.launchCore}\\b`, 'i');
			const preferred = running.find(t => idRe.test(t.info.ID) || (t.info.Name ? nameRe.test(t.info.Name) : false));
			if (preferred) {
				return preferred.info.ID;
			}
		}
		return running[0].info.ID;
	}

	private refreshLiveWatchTimer(): void {
		if (this.liveWatchTimer) {
			clearInterval(this.liveWatchTimer);
			this.liveWatchTimer = undefined;
		}
	}

	private async onLiveWatchTimerTick(): Promise<void> {
		return;
	}

	private setLiveWatchState(enabled: boolean, intervalMs?: number): void {
		this.liveWatchEnabled = enabled;
		if (intervalMs !== undefined) {
			this.liveWatchIntervalMs = this.normalizeLiveWatchIntervalMs(intervalMs);
		}
		this.refreshLiveWatchTimer();
	}

	private async evaluateGlobalExpressionValue(expressionText: string): Promise<string | undefined> {
		const runningCtx = await this.pickContextIdForRunningWatch();
		const ctx = runningCtx
			?? (Date.now() < this.liveWatchSuspendFallbackBlockedUntilMs ? undefined : await this.pickAnySuspendedContextId());
		if (!ctx) {
			return undefined;
		}
		const exprId = await this.tcfClient.sendCommand(new CreateExpressionsCommand(ctx, null, expressionText));
		const v = await this.tcfClient.getExpressionIDVariable(ctx, ctx, exprId, expressionText);
		if (!v) {
			return undefined;
		}
		return (await v.displayValue?.()) ?? '';
	}


	protected override async continueRequestAsync(response: DebugProtocol.ContinueResponse, args: DebugProtocol.ContinueArguments): Promise<void> {
		const contextId = this.threadIdToContext.get(args.threadId);
		await this.tcfClient.continue(contextId);
		this.sendResponse(response);
	}

	protected override async nextRequestAsync(response: DebugProtocol.NextResponse, args: DebugProtocol.NextArguments): Promise<void> {
		const contextId = this.threadIdToContext.get(args.threadId);
		this.pendingStepThreads.add(args.threadId);
		if (this.instructionSteppingEnabled) {
			await this.tcfClient.nextInstruction(contextId);
		} else {
			await this.tcfClient.next(contextId);
		}
		this.sendResponse(response);
	}

	/** Resolves "0x80001234", "2147487796", "symbol" or "symbol+0x10" to an address in the program code. */
	private async resolveDisassemblyTarget(ctx: string, text: string): Promise<{ address: number; label: string }> {
		const parseNumber = (s: string): number | undefined => {
			const t = s.trim().replace(/[uU]?[lL]*$/, '');
			if (/^0x[0-9a-f]+$/i.test(t)) {
				return Number.parseInt(t.substring(2), 16);
			}
			return /^[0-9]+$/.test(t) ? Number.parseInt(t, 10) : undefined;
		};
		const input = text.trim();
		if (input === '') {
			throw new Error('no address or symbol given');
		}

		let address = parseNumber(input);
		let label: string | undefined;
		if (address === undefined) {
			const m = /^&?\s*([A-Za-z_$.][\w$.]*)\s*(?:([+-])\s*(\S+))?$/.exec(input);
			const offset = m?.[3] !== undefined ? parseNumber(m[3]) : 0;
			if (!m || offset === undefined) {
				throw new Error(`"${input}" is neither an address nor a symbol`);
			}
			let base = this.getElfFunctionSymbols().find(s => s.name === m[1])?.address;
			const ids = base === undefined
				? await this.tcfClient.sendCommand(new FindByNameSymbolsCommand(ctx, 0, m[1])).catch(() => [] as string[])
				: [];
			for (const id of ids) {
				const sym = await this.tcfClient.sendCommand(new GetContextSymbolsCommand(id)).catch(() => undefined);
				if (typeof sym?.Address === 'number') {
					base = sym.Address;
					break;
				}
			}
			if (base === undefined) {
				throw new Error(`symbol "${m[1]}" not found`);
			}
			address = m[2] === '-' ? base - offset : base + offset;
			label = offset ? `${m[1]}${m[2]}0x${offset.toString(16)}` : m[1];
		}

		// TriCore instructions start on even addresses.
		address -= address % 2;
		if (this.execRanges.length > 0 && !this.findExecutableRangeForAddress(address)) {
			throw new Error(`0x${address.toString(16)} is not in an executable section of the program`);
		}
		label ??= await this.symbolLabelForAddress(ctx, address);
		return { address, label: `0x${address.toString(16)}${label ? ` <${label}>` : ''}` };
	}

	private async symbolLabelForAddress(ctx: string, address: number): Promise<string | undefined> {
		try {
			const id = await this.tcfClient.sendCommand(new FindByAddrSymbolsCommand(ctx, address));
			const sym = await this.tcfClient.sendCommand(new GetContextSymbolsCommand(id));
			if (!sym?.Name) {
				return undefined;
			}
			const offset = typeof sym.Address === 'number' ? address - sym.Address : 0;
			return offset > 0 ? `${sym.Name}+0x${offset.toString(16)}` : sym.Name;
		} catch {
			return undefined;
		}
	}

	private findExecutableRangeForAddress(addr: number): { start: number; end: number } | undefined {
		if (this.execRanges.length === 0) {
			return undefined;
		}
		for (const r of this.execRanges) {
			if (addr >= r.start && addr < r.end) {
				return r;
			}
		}
		return undefined;
	}

	private getElfFunctionSymbols(): Array<{ name: string; address: number }> {
		if (!this.elfFunctionSymbols) {
			try {
				this.elfFunctionSymbols = this.programPath ? this.loadFunctionSymbolsFromElf(this.programPath) : [];
			} catch (e) {
				this.tcfLogger.error(`Failed to read ELF symbols: ${String(e)}`);
				this.elfFunctionSymbols = [];
			}
		}
		return this.elfFunctionSymbols;
	}

	/** Reads the function symbols (STT_FUNC) from the ELF .symtab, sorted by name. */
	private loadFunctionSymbolsFromElf(elfPath: string): Array<{ name: string; address: number }> {
		const buf = fs.readFileSync(elfPath);
		if (buf.length < 0x40 || !(buf[0] === 0x7f && buf[1] === 0x45 && buf[2] === 0x4c && buf[3] === 0x46)) {
			throw new Error('Not an ELF file');
		}
		const is32 = buf[4] === 1;
		const little = buf[5] === 1;
		const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
		const readU16 = (off: number) => view.getUint16(off, little);
		const readU32 = (off: number) => view.getUint32(off, little);
		const readAddr = (off: number) => (is32 ? readU32(off) : Number(view.getBigUint64(off, little)));

		const shoff = readAddr(is32 ? 32 : 40);
		const shentsize = readU16(is32 ? 46 : 58);
		const shnum = readU16(is32 ? 48 : 60);
		const section = (i: number) => {
			const off = shoff + i * shentsize;
			return {
				type: readU32(off + 4),
				offset: readAddr(is32 ? off + 16 : off + 24),
				size: readAddr(is32 ? off + 20 : off + 32),
				link: readU32(is32 ? off + 24 : off + 40),
				entsize: readAddr(is32 ? off + 36 : off + 56),
			};
		};

		const byName = new Map<string, number>();
		for (let i = 0; i < shnum && shoff + (i + 1) * shentsize <= buf.length; i++) {
			const symtab = section(i);
			if (symtab.type !== 2 /* SHT_SYMTAB */ || symtab.entsize <= 0 || symtab.link >= shnum) {
				continue;
			}
			const strtab = section(symtab.link);
			const count = Math.floor(Math.min(symtab.size, buf.length - symtab.offset) / symtab.entsize);
			for (let n = 0; n < count; n++) {
				const off = symtab.offset + n * symtab.entsize;
				const nameOff = readU32(off);
				const info = is32 ? buf[off + 12] : buf[off + 4];
				const value = is32 ? readU32(off + 4) : Number(view.getBigUint64(off + 8, little));
				if ((info & 0xf) !== 2 /* STT_FUNC */ || nameOff === 0 || value === 0) {
					continue;
				}
				const start = strtab.offset + nameOff;
				const end = buf.indexOf(0, start);
				const name = buf.toString('utf8', start, end < 0 ? buf.length : end);
				if (name && !byName.has(name)) {
					byName.set(name, value);
				}
			}
		}
		return Array.from(byName, ([name, address]) => ({ name, address })).sort((a, b) => a.name.localeCompare(b.name));
	}

	private loadExecutableRangesFromElf(elfPath: string): Array<{ start: number; end: number }> {
		const buf = fs.readFileSync(elfPath);
		if (buf.length < 0x40) {
			throw new Error('ELF file too small');
		}
		if (!(buf[0] === 0x7f && buf[1] === 0x45 && buf[2] === 0x4c && buf[3] === 0x46)) {
			throw new Error('Not an ELF file');
		}
		const eiClass = buf[4]; // 1=32-bit, 2=64-bit
		const eiData = buf[5]; // 1=little, 2=big
		const little = eiData === 1;
		const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);

		const readU16 = (off: number) => view.getUint16(off, little);
		const readU32 = (off: number) => view.getUint32(off, little);
		const readU64 = (off: number) => Number(view.getBigUint64(off, little));

		let phoff = 0;
		let phentsize = 0;
		let phnum = 0;
		if (eiClass === 1) {
			// ELF32
			phoff = readU32(28);
			phentsize = readU16(42);
			phnum = readU16(44);
		} else if (eiClass === 2) {
			// ELF64
			phoff = readU64(32);
			phentsize = readU16(54);
			phnum = readU16(56);
		} else {
			throw new Error(`Unknown ELF class: ${eiClass}`);
		}

		if (phoff <= 0 || phentsize <= 0 || phnum <= 0) {
			return [];
		}

		const ranges: Array<{ start: number; end: number }> = [];
		for (let i = 0; i < phnum; i++) {
			const off = phoff + i * phentsize;
			if (off + phentsize > buf.length) {
				break;
			}
			let pType = 0;
			let pFlags = 0;
			let pVaddr = 0;
			let pMemsz = 0;
			if (eiClass === 1) {
				pType = readU32(off + 0);
				pFlags = readU32(off + 24);
				pVaddr = readU32(off + 8);
				pMemsz = readU32(off + 20);
			} else {
				pType = readU32(off + 0);
				pFlags = readU32(off + 4);
				pVaddr = readU64(off + 16);
				pMemsz = readU64(off + 40);
			}
			// PT_LOAD = 1, PF_X = 0x1
			if (pType === 1 && (pFlags & 0x1) !== 0 && pMemsz > 0) {
				const start = pVaddr;
				const end = pVaddr + pMemsz;
				if (Number.isFinite(start) && Number.isFinite(end) && end > start) {
					ranges.push({ start, end });
				}
			}
		}

		// Merge overlapping/adjacent ranges.
		ranges.sort((a, b) => a.start - b.start);
		const merged: Array<{ start: number; end: number }> = [];
		for (const r of ranges) {
			const last = merged[merged.length - 1];
			if (!last) {
				merged.push({ ...r });
				continue;
			}
			if (r.start <= last.end) {
				last.end = Math.max(last.end, r.end);
			} else {
				merged.push({ ...r });
			}
		}
		return merged;
	}

	protected override async stepInRequestAsync(response: DebugProtocol.StepInResponse, args: DebugProtocol.StepInArguments): Promise<void> {
		const contextId = this.threadIdToContext.get(args.threadId);
		this.pendingStepThreads.add(args.threadId);
		if (this.instructionSteppingEnabled) {
			await this.tcfClient.nextInstruction(contextId);
		} else {
			await this.tcfClient.stepIn(contextId);
		}
		this.sendResponse(response);
	}

	protected override async stepOutRequestAsync(response: DebugProtocol.StepOutResponse, args: DebugProtocol.StepOutArguments): Promise<void> {
		const contextId = this.threadIdToContext.get(args.threadId);
		this.pendingStepThreads.add(args.threadId);
		// No instruction-level "step out" in TCF; keep semantic step-out.
		await this.tcfClient.stepOut(contextId);
		this.sendResponse(response);
	}

	protected override async pauseRequestAsync(response: DebugProtocol.PauseResponse, args: DebugProtocol.PauseArguments): Promise<void> {
		// Pause always stops the whole multi-core system, but only one core gets the focus (and its
		// source opened):
		//  - pause button of a core in the Call Stack view: that core;
		//  - toolbar pause button (VS Code sends the focused thread): the launch core (core 0).
		const requested = this.threadIdToContext.get(args.threadId);
		const fromToolbar = this.activeThreadId === undefined || args.threadId === this.activeThreadId;
		const focusCtx = fromToolbar ? (await this.pickContextIdForCore(this.launchCore ?? 0)) ?? requested : requested;
		this.focusLog(`pauseRequest threadId=${args.threadId} active=${String(this.activeThreadId)} toolbar=${fromToolbar} focus=${String(focusCtx)}`);
		this.cancelPendingPauseStoppedTimers('pauseRequest');
		this.userPauseUntilMs = Number.POSITIVE_INFINITY;
		try {
			await this.pauseAllCoresBestEffort('pauseRequestAsync');
			// The focus core may be reported suspended a moment later than the pause returns; without
			// it no stopped event would take the focus (the other cores' events preserve it).
			if (focusCtx) {
				for (let i = 0; i < 10; i++) {
					const infos = await this.tcfClient.getThreads();
					if (infos.some(t => t.info.ID === focusCtx && Boolean(t.state?.suspended))) {
						break;
					}
					await new Promise(resolve => setTimeout(resolve, 50));
				}
			}
			await this.emitStoppedForAllSuspendedAtStartup(focusCtx, { targetReason: StoppedEventReason.pause, otherReason: StoppedEventReason.pause });
			// VS Code cannot always open the frame's source itself (mapped paths); open it like a
			// breakpoint stop does, for the focus core only.
			if (focusCtx) {
				void this.emitSourceLocation(focusCtx);
			}
		} finally {
			// Late suspend events of the other cores must not take the focus either.
			this.userPauseUntilMs = Date.now() + 500;
		}
		this.sendResponse(response);
	}

	private isLaunchCoreContext(contextId: string): boolean {
		const m = /(?:^|\.)C(\d+)/.exec(contextId);
		return m !== null && Number(m[1]) === (this.launchCore ?? 0);
	}

	protected override async setBreakPointsRequestAsync(response: DebugProtocol.SetBreakpointsResponse, args: DebugProtocol.SetBreakpointsArguments): Promise<void> {
		// Breakpoint flow (VS Code -> DAP - > TCF):
		// 1) VS Code sends SetBreakpointsRequest with file + lines.
		// 2) Update local state (add/remove as needed) and assign IDs.
		// 3) If already configured, sync all breakpoints to TCF agent.
		// 4) Respond to VS Code with the actual breakpoint states so UI updates.
		const file = args.source.path || '';
		const lines = (args.breakpoints || []).map(b => b.line);
		const tuples = this.breakpointsManager.updateFile(file, lines);

		if (this.breakpointsManager.configured) {
			await this.syncAllBreakpoints();
		}

		const bps = tuples.map(t => {
			const bp = new Breakpoint(t.verified, t.line);
			(bp as any).id = t.id;
			return bp;
		});
		response.body = { breakpoints: bps };
		this.sendResponse(response);
	}

	protected override async setInstructionBreakpointsRequestAsync(
		response: DebugProtocol.SetInstructionBreakpointsResponse,
		args: DebugProtocol.SetInstructionBreakpointsArguments
	): Promise<void> {
		const requestAddrs: Array<number | undefined> = [];
		const addrs: number[] = [];

		for (const bp of args.breakpoints || []) {
			const base = this.parseMemoryReference(bp.instructionReference);
			if (base === undefined) {
				requestAddrs.push(undefined);
				continue;
			}
			const offset = typeof bp.offset === 'number' ? bp.offset : 0;
			const addr = base + offset;
			requestAddrs.push(addr);
			addrs.push(addr);
		}

		if (this.startupDiagnosticsEnabled) {
			const refs = requestAddrs.map(addr => (addr === undefined ? 'invalid' : `0x${addr.toString(16)}`));
			this.tcfLogger.log(`[dap] SetInstructionBreakpointsRequest count=${requestAddrs.length} addrs=${refs.join(', ')}`);
		}

		const tuples = this.instructionBreakpointsManager.updateAddresses(addrs, () => this.breakpointsManager.allocateId());
		if (this.breakpointsManager.configured) {
			await this.syncAllBreakpoints();
		}

		const byAddr = new Map<number, InstructionBreakpointTuple>();
		for (const t of tuples) {
			byAddr.set(t.address, t);
		}

		const bps = requestAddrs.map((addr) => {
			if (addr === undefined) {
				const bp = new Breakpoint(false, 0);
				(bp as any).message = 'Invalid instructionReference';
				return bp;
			}
			const t = byAddr.get(addr);
			const bp = new Breakpoint(t?.verified ?? false, 0);
			if (t) {
				(bp as any).id = t.id;
			}
			(bp as any).instructionReference = `0x${addr.toString(16)}`;
			return bp;
		});

		response.body = { breakpoints: bps };
		this.sendResponse(response);
	}

	private breakpointIdToTcfId(id: number): string {
		return `${this.breakpointPrefix}${id}`;
	}

	private breakpointTcfIdToId(tcfId: string): number {
		if (!tcfId.startsWith(this.breakpointPrefix)) {
			return BREAKPOINT_INVALID;
		}
		const num = Number(tcfId.substring(this.breakpointPrefix.length));
		return Number.isFinite(num) ? num : BREAKPOINT_INVALID;
	}

	private async syncAllBreakpoints(): Promise<void> {
		const all = this.breakpointsManager.getAll();
		const data: BreakpointData[] = all.map(b => ({
			ID: this.breakpointIdToTcfId(b.id),
			Enabled: true,
			File: this.convertClientPathToDebugger(b.file),
			Line: b.line,
		}));
		const instr = this.instructionBreakpointsManager.getAll();
		let instrContextIds: string[] | undefined;
		if (instr.length > 0) {
			try {
				const target = (await this.pickContextIdForCore(this.launchCore)) ?? (await this.pickAnySuspendedContextId());
				if (target) {
					instrContextIds = [target];
				} else {
					const infos = await this.tcfClient.getThreads();
					instrContextIds = infos
						.filter(t => Boolean(t.info?.CanSuspend))
						.map(t => t.info.ID);
				}
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(
						`[breakpoints] instr ContextIds=${instrContextIds.length ? instrContextIds.join(', ') : '(none)'}`
					);
				}
			} catch (e) {
				if (this.startupDiagnosticsEnabled) {
					this.tcfLogger.log(`[breakpoints] instr ContextIds query failed: ${String(e)}`);
				}
			}
		}
		if (this.startupDiagnosticsEnabled) {
			const addrStrings = instr.map(b => `0x${b.address.toString(16)}`);
			const maxShow = 16;
			const shown = addrStrings.slice(0, maxShow);
			const suffix = addrStrings.length > maxShow ? ` ...+${addrStrings.length - maxShow}` : '';
			this.tcfLogger.log(
				`[breakpoints] syncAllBreakpoints files=${all.length} instr=${instr.length} instrAddrs=${shown.join(', ')}${suffix}`
			);
		}
		for (const b of instr) {
			const location = `0x${b.address.toString(16)}`;
			data.push({
				ID: this.breakpointIdToTcfId(b.id),
				Enabled: true,
				Address: location,
				Location: location,
				BreakpointType: BreakpointType.Hardware,
				ContextIds: instrContextIds && instrContextIds.length > 0 ? instrContextIds : undefined,
			});
		}
		await this.tcfClient.setBreakpointsList(data);
	}

	/** C expression of a child variable shown under a parent expression. */
	private static childExpression(parentExpr: string, childName: string): string {
		const simple = /^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/.test(parentExpr);
		const p = simple ? parentExpr : `(${parentExpr})`;
		if (childName === '*') {
			return `*${p}`;
		}
		if (/^\[\d+\]$/.test(childName)) {
			return `${p}${childName}`;
		}
		return `${p}.${childName}`;
	}

	/** Remembers a variable's expression and evaluation scope, so it can be assigned later. */
	private static annotateVariable(v: any, expression: string | undefined, scope: string | undefined): void {
		if (v && expression && scope && v.__aurixExpr === undefined) {
			v.__aurixExpr = expression;
			v.__aurixScope = scope;
		}
	}

	private static annotateChildren(parent: any, children: any[]): void {
		if (!parent?.__aurixExpr || !parent?.__aurixScope) {
			return;
		}
		for (const c of children) {
			const name = c?.name?.();
			if (typeof name === 'string' && name !== '') {
				AurixTCFDebugSession.annotateVariable(c, AurixTCFDebugSession.childExpression(parent.__aurixExpr, name), parent.__aurixScope);
			}
		}
	}

	private async toDAPVariable(v: any, parentRef: number): Promise<DebugProtocol.Variable> {
		const name = v.name?.() ?? '<unnamed>';
		const value = (await v.displayValue?.()) ?? '';
		let variablesReference = 0;
		try {
			const children = await v.getChildren?.();
			if (children && children.length > 0) {
				AurixTCFDebugSession.annotateChildren(v, children);
				variablesReference = this.createSubVariableReference(parentRef, children);
			}
		} catch {
			// ignore child errors
		}
		return new Variable(name, value, variablesReference);
	}

	private onBreakpointHit(tcfId: string, status: InstanceStatusData[]) {
		const id = this.breakpointTcfIdToId(tcfId);
		if (id === BREAKPOINT_INVALID) {
			return;
		}
		const contexts = status.map(s => s.LocationContext).filter((c): c is string => Boolean(c));
		if (contexts.length === 0) {
			return;
		}

		const contextId = contexts[0];
		if (this.pendingBreakpointStoppedTimers.has(contextId)) {
			return;
		}
		this.cancelPendingPauseStoppedTimers('breakpointHit');
		this.suppressPauseStoppedUntilMs = Date.now() + Math.max(150, this.stopStormPauseDelayMs * 4);

		const timer = setTimeout(() => {
			this.pendingBreakpointStoppedTimers.delete(contextId);
			const threadId = this.getOrAssignThreadId(contextId);
			this.onThreadSuspended(threadId);
			this.notePrimaryBreakpointStop(threadId, contextId);
			this.sendStoppedEvent(StoppedEventReason.breakpoint, threadId, { allThreadsStopped: true });
			void this.emitSourceLocation(contextId);
		}, this.breakpointStopDelayMs);
		this.pendingBreakpointStoppedTimers.set(contextId, timer);
	}

	private onBreakpointRemoved(tcfIds: string[]) {
		for (const tcfId of tcfIds) {
			const id = this.breakpointTcfIdToId(tcfId);
			if (id === BREAKPOINT_INVALID) {
				continue;
			}
			this.breakpointsManager.deleteById(id);
			this.instructionBreakpointsManager.deleteById(id);
			this.sendEvent(new BreakpointEvent(BreakpointEventReason.removed, { id, verified: false }));
		}
	}

	private onBreakpointAddedOrChanged(tcfIds: string[]) {
		for (const tcfId of tcfIds) {
			const id = this.breakpointTcfIdToId(tcfId);
			if (id === BREAKPOINT_INVALID) {
				continue;
			}
			this.breakpointsManager.setVerified(id, true);
			this.instructionBreakpointsManager.setVerified(id, true);
			this.sendEvent(new BreakpointEvent(BreakpointEventReason.changed, { id, verified: true }));
		}
	}

	private onContextSuspended(info: ContextSuspendedData) {
		// Ignore duplicate suspend events for the same context.
		const contextId = info.id;
		if (this.suspendedContextIds.has(contextId)) {
			return;
		}
		this.suspendedContextIds.add(contextId);
		const threadId = this.getOrAssignThreadId(contextId);
		this.onThreadSuspended(threadId);

		const reasonText = (info.reason || '').toLowerCase();
		this.focusLog(`contextSuspended ctx=${contextId} threadId=${threadId} reason=${String(info.reason ?? '')} pc=0x${Number(info.pc ?? 0).toString(16)}`);
		const pendingStep = this.pendingStepThreads.delete(threadId);
		let reason = reasonText.includes('breakpoint') ? StoppedEventReason.breakpoint : StoppedEventReason.pause;
		if (reason !== StoppedEventReason.breakpoint) {
			if (pendingStep || reasonText.includes('step')) {
				reason = StoppedEventReason.step;
			}
		}
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(`[runcontrol] suspended id=${contextId} reason=${String(info.reason ?? '')}`);
		}
		if (reason === StoppedEventReason.breakpoint) {
			this.cancelPendingBreakpointStoppedTimer(contextId, 'runcontrolBreakpoint');
		}
		if (this.startupDiagnosticsEnabled && reason === StoppedEventReason.breakpoint) {
			const instr = this.instructionBreakpointsManager.getAll();
			if (instr.length > 0) {
				const pc = info.pc;
				const bpIds = Array.isArray(info.data?.BPs) ? info.data?.BPs : [];
				if (bpIds.length > 0) {
					const details = bpIds.map((tcfId) => {
						const id = this.breakpointTcfIdToId(tcfId);
						if (id === BREAKPOINT_INVALID) {
							return `${tcfId}(unknown)`;
						}
						const instrBp = this.instructionBreakpointsManager.getById(id);
						if (instrBp) {
							return `${tcfId}=instr@0x${instrBp.address.toString(16)}`;
						}
						const srcBp = this.breakpointsManager.getById(id);
						if (srcBp) {
							const file = srcBp.file ? srcBp.file.split(/[\\/]/).pop() : '';
							return `${tcfId}=src ${file}:${srcBp.line}`;
						}
						return `${tcfId}=untracked`;
					});
					this.tcfLogger.log(`[breakpoints] stop bps=${details.join(', ')}`);
				} else {
					this.tcfLogger.log(`[breakpoints] stop bps=(none) data=${JSON.stringify(info.data ?? {})}`);
				}
				let match: InstructionBreakpointTuple | undefined;
				let nearest: InstructionBreakpointTuple | undefined;
				let nearestDelta = Number.POSITIVE_INFINITY;
				for (const b of instr) {
					if (b.address === pc) {
						match = b;
						break;
					}
					const delta = Math.abs(b.address - pc);
					if (delta < nearestDelta) {
						nearestDelta = delta;
						nearest = b;
					}
				}
				const matchText = match ? `id=${match.id}` : 'none';
				const nearestText = nearest ? `0x${nearest.address.toString(16)} (id=${nearest.id}, delta=${nearestDelta})` : 'none';
				this.tcfLogger.log(`[breakpoints] stop pc=0x${pc.toString(16)} instrMatch=${matchText} nearest=${nearestText}`);
			}
		}
		if (reason === StoppedEventReason.breakpoint) {
			this.refreshLiveWatchTimer();
			this.cancelPendingPauseStoppedTimers('runcontrolBreakpoint');
			this.suppressPauseStoppedUntilMs = Date.now() + Math.max(150, this.stopStormPauseDelayMs * 4);
			this.notePrimaryBreakpointStop(threadId, contextId);
			this.sendStoppedEvent(reason, threadId, { allThreadsStopped: true });
			void this.emitSourceLocation(contextId);
			return;
		}

		// Step is an explicit user action; don't delay or suppress it.
		if (reason === StoppedEventReason.step) {
			this.refreshLiveWatchTimer();
			this.sendStoppedEvent(reason, threadId);
			void this.emitSourceLocation(contextId);
			return;
		}

		// TL1 stops all cores together, so a pause-type stop takes the focus (and opens a source) for
		// one core only, otherwise VS Code jumps between the sources of all cores. During an explicit
		// pause, pauseRequestAsync focuses the chosen core itself; otherwise the launch core does.
		if (Date.now() < this.userPauseUntilMs) {
			this.focusLog(`pause StoppedEvent without focus for ${contextId} (threadId=${threadId}, user pause)`);
			this.sendStoppedEvent(StoppedEventReason.pause, threadId, { preserveFocus: true });
			this.refreshLiveWatchTimer();
			return;
		}
		const takesFocus = this.isLaunchCoreContext(contextId);

		// Delay pause-type stops slightly. This helps ensure a breakpoint stop becomes the primary
		// UI context when multiple cores suspend nearly simultaneously.
		if (Date.now() < this.suppressPauseStoppedUntilMs) {
			this.focusLog(`suppress pause StoppedEvent for ${contextId} (threadId=${threadId})`);
			return;
		}
		if (this.stopStormPauseDelayMs > 0) {
			if (this.pendingPauseStoppedTimers.has(contextId)) {
				return;
			}
			const timer = setTimeout(() => {
				this.pendingPauseStoppedTimers.delete(contextId);
				if (!this.suspendedContextIds.has(contextId)) {
					return;
				}
				if (Date.now() < this.suppressPauseStoppedUntilMs || Date.now() < this.userPauseUntilMs) {
					return;
				}
				this.sendStoppedEvent(StoppedEventReason.pause, threadId, { preserveFocus: !takesFocus });
				if (takesFocus) {
					void this.emitSourceLocation(contextId);
				}
			}, this.stopStormPauseDelayMs);
			this.pendingPauseStoppedTimers.set(contextId, timer);
			return;
		}

		this.sendStoppedEvent(reason, threadId, { preserveFocus: !takesFocus });
		this.refreshLiveWatchTimer();
		if (takesFocus) {
			void this.emitSourceLocation(contextId);
		}
	}

	private onContextResumed(contextId: string) {
		this.suspendedContextIds.delete(contextId);
		const threadId = this.getOrAssignThreadId(contextId);
		this.onThreadResumed(threadId);
		this.disassemblyJumpTarget = undefined;
		// Important for multi-core: do NOT let VS Code assume all threads continued.
		this.sendEvent(new ContinuedEvent(threadId, false));
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(`[dap] ContinuedEvent threadId=${threadId} ctx=${contextId} allThreadsContinued=false`);
		}
		if (this.startupDiagnosticsEnabled) {
			this.tcfLogger.log(`[runcontrol] resumed id=${contextId} threadId=${threadId}`);
		}
		this.refreshLiveWatchTimer();
	}

	protected override async customRequestAsync(command: string, response: DebugProtocol.Response, args: any, request?: DebugProtocol.Request): Promise<void> {
		if (command === 'aurix-tcf/activeThread') {
			this.activeThreadId = typeof args?.threadId === 'number' ? args.threadId : undefined;
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/setDisassemblyOptions') {
			if (isDisassemblyOpcodeBytes(args?.opcodeBytes)) {
				this.disassemblyOpcodeBytes = args.opcodeBytes;
			}
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/disassemblySymbols') {
			response.body = { symbols: this.getElfFunctionSymbols().map(s => ({ name: s.name, address: `0x${s.address.toString(16)}` })) };
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/disassemblyGoTo') {
			try {
				const ctx = await this.pickAnySuspendedContextId();
				if (!ctx) {
					throw new Error('the target is running; halt it first');
				}
				const target = await this.resolveDisassemblyTarget(ctx, String(args?.target ?? ''));
				this.disassemblyJumpTarget = target;
				this.sendEvent(new InvalidatedEvent(['stacks']));
				response.body = { address: `0x${target.address.toString(16)}`, label: target.label };
				this.sendResponse(response);
			} catch (e) {
				this.sendErrorResponse(response, 0, '{message}', { message: AurixTCFDebugSession.formatError(e) });
			}
			return;
		}

		if (command === 'aurix-tcf/setInstructionStepping') {
			this.instructionSteppingEnabled = Boolean(args?.enabled);
			this.sendResponse(response);
			return;
		}
		if (command === 'aurix-tcf/halt') {
			try {
				// Best-effort: suspend the selected core context first (faster than trying many contexts).
				const ctx = await this.pickContextIdForCore(this.launchCore);
				if (ctx) {
					await this.tcfClient.pause(ctx);
				} else {
					await this.pauseAllCoresBestEffort('halt');
				}
				// Give the agent a moment to publish suspended state, then push a stopped event.
				await new Promise(resolve => setTimeout(resolve, 200));
				const ctx2 = await this.pickContextIdForCore(this.launchCore);
				if (ctx2) {
					await this.emitStoppedIfSuspended(ctx2, StoppedEventReason.pause);
				}
			} catch (e) {
				this.tcfLogger.error(`Halt failed: ${String(e)}`);
			}
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/flash') {
			try {
				const files = AurixTCFDebugSession.getFlashFiles(this.launchArgs);
				if (this.agentFlavor !== 'aurix-dbg') {
					throw new Error('flashing from the debug toolbar needs agentFlavor "aurix-dbg"');
				}
				if (files.length === 0) {
					throw new Error('no "flashFiles" or "program" in the launch configuration');
				}
				await this.flashInSession(files);
				this.sendResponse(response);
			} catch (e) {
				this.sendErrorResponse(response, 1, '{message}', { message: AurixTCFDebugSession.formatError(e) });
			}
			return;
		}

		// Tcl command from a launch configuration "actions" entry (Debugger Actions toolbar button).
		if (command === 'aurix-tcf/tcl') {
			const script = typeof args?.script === 'string' ? args.script.trim() : '';
			if (!script) {
				this.sendErrorResponse(response, 0, 'No Tcl command given');
				return;
			}
			if (!this.tcfClient.supportedServices?.includes(TCL_SERVICE)) {
				this.sendErrorResponse(response, 0, 'This TCF agent has no Tcl console (debugger actions need aurix-dbg)');
				return;
			}
			const timeoutMs = typeof args?.timeoutMs === 'number' && args.timeoutMs > 0 ? args.timeoutMs : undefined;
			const oldTimeout = (this.tcfClient as any).commandTimeout;
			try {
				if (timeoutMs) {
					this.tcfClient.setCommandTimeout(timeoutMs);
				}
				this.sendEvent(new OutputEvent(`> ${script}\n`, 'console'));
				const out = await this.tcfClient.sendCommand(new TclEvalCommand(script));
				if (out) {
					this.sendEvent(new OutputEvent(`${out}\n`, 'console'));
				}
				response.body = { result: out ?? '' };
				this.sendResponse(response);
			} catch (e) {
				const message = AurixTCFDebugSession.formatError(e);
				this.sendEvent(new OutputEvent(`${message}\n`, 'stderr'));
				this.sendErrorResponse(response, 0, '{message}', { message });
			} finally {
				if (timeoutMs && typeof oldTimeout === 'number') {
					this.tcfClient.setCommandTimeout(oldTimeout);
				}
			}
			return;
		}

		if (command === 'aurix-tcf/reset') {
			try {
				this.beginResetTransition();
				// NOTE: ContextReset.getCapabilities is unreliable on some AURIX agents (can time out).
				// We therefore skip capability probing and use a configured resetType or a conservative default.
				const requestedType = typeof args?.type === 'string' ? args.type.trim() : '';
				const candidateTypes = [requestedType, this.launchResetType ?? '', 'reset']
					.map(t => (typeof t === 'string' ? t.trim() : ''))
					.filter(t => t !== '')
					.filter((t, i, a) => a.indexOf(t) === i);

				// Default: run after reset (agent should stop at first breakpoint / stop-at-function).
				const suspend = typeof args?.suspend === 'boolean' ? args.suspend : this.launchResetSuspend;
				const requestedCore = typeof args?.core === 'number' ? args.core : this.launchCore;
				const ctx = await this.pickContextIdForCore(requestedCore);
				if (!ctx) {
					throw new Error('No target context available for reset.');
				}
				// A running reset is paused afterwards unless the caller says otherwise; a halting reset
				// (resetSuspend) is already stopped.
				const pauseAllAfterReset = typeof args?.pauseAll === 'boolean' ? args.pauseAll : !suspend;

				// Temporarily increase timeout for reset; reset can legitimately take longer.
				const oldTimeout = (this.tcfClient as any).commandTimeout;
				try {
					this.tcfClient.setCommandTimeout(Math.max(30_000, Number(oldTimeout) || 0));

					let lastError: unknown;
					for (const type of candidateTypes) {
						try {
							this.tcfLogger.log(`Reset: type='${type}', suspend=${suspend}`);
							await this.tcfClient.reset(ctx, type, suspend);
							lastError = undefined;
							break;
						} catch (e) {
							lastError = e;
							let errText = '';
							if (e instanceof Error) {
								errText = e.message;
							} else {
								try {
									errText = JSON.stringify(e);
								} catch {
									errText = String(e);
								}
							}
							this.tcfLogger.error(`Reset attempt failed for type='${type}': ${errText}`);
						}
					}
					if (lastError) {
						throw lastError;
					}

					if (pauseAllAfterReset) {
						// Some resets will cause one core to start running immediately.
						// If the user requested a paused post-reset state, explicitly pause all cores.
						await new Promise(resolve => setTimeout(resolve, 200));
						try {
							await this.pauseAllCoresBestEffort('post-reset');
						} catch (e) {
							this.tcfLogger.error(`Post-reset pause-all failed: ${String(e)}`);
						}
						this.beginResetTransition();
						await new Promise(resolve => setTimeout(resolve, 200));
						await this.emitStoppedForAllSuspendedAtStartup(ctx, { targetReason: StoppedEventReason.pause, otherReason: StoppedEventReason.pause });
					}
				} finally {
					if (typeof oldTimeout === 'number') {
						this.tcfClient.setCommandTimeout(oldTimeout);
					}
				}
			} catch (e) {
				this.tcfLogger.error(`Reset failed: ${String(e)}`);
			}
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/resetCapabilities') {
			// NOTE: ContextReset.getCapabilities is unreliable on some AURIX agents and can
			// hang until the command timeout (see the reset handler above, which avoids it
			// entirely). Use a short dedicated timeout here so this diagnostic fails fast
			// instead of silently sitting there for 30+ seconds.
			const oldTimeout = (this.tcfClient as any).commandTimeout;
			try {
				this.tcfClient.setCommandTimeout(5_000);
				const caps = await this.tcfClient.getResetCapabilities();
				this.tcfLogger.log(
					caps.length > 0
						? `ContextReset.getCapabilities: ${caps.map(c => `${c.Type}${c.Description ? ` (${c.Description})` : ''}`).join(', ')}`
						: 'ContextReset.getCapabilities returned no reset types.'
				);
				response.body = { capabilities: caps };
			} catch (e) {
				this.tcfLogger.error(`ContextReset.getCapabilities failed: ${String(e)}`);
				response.body = { capabilities: [], error: String(e) };
			} finally {
				if (typeof oldTimeout === 'number') {
					this.tcfClient.setCommandTimeout(oldTimeout);
				}
			}
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/addGlobal') {
			const expression = typeof args?.expression === 'string' ? args.expression.trim() : '';
			if (expression !== '' && !this.configuredGlobals.includes(expression)) {
				this.configuredGlobals.push(expression);
			}
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/setLiveWatch') {
			this.setLiveWatchState(Boolean(args?.enabled), args?.intervalMs);
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/liveWatchEvaluate') {
			const expressions = Array.isArray(args?.expressions)
				? args.expressions.filter((value: unknown): value is string => typeof value === 'string' && value.trim() !== '')
				: [];
			const values = [] as Array<{ expression: string; result: string; available: boolean; error?: string }>;
			for (const expression of expressions) {
				try {
					const result = await this.evaluateGlobalExpressionValue(expression);
					values.push({
						expression,
						result: result ?? '',
						available: result !== undefined,
					});
				} catch (e) {
					values.push({
						expression,
						result: '',
						available: false,
						error: e instanceof Error ? e.message : String(e),
					});
				}
			}
			response.body = { values };
			this.sendResponse(response);
			return;
		}

		if (command === 'aurix-tcf/liveWatchWrite') {
			const expression = typeof args?.expression === 'string' ? args.expression.trim() : '';
			const value = typeof args?.value === 'string' ? args.value : String(args?.value ?? '');
			if (!expression) {
				this.sendErrorResponse(response, 0, 'No expression given');
				return;
			}
			try {
				const scope = (await this.pickContextIdForRunningWatch()) ?? (await this.pickAnySuspendedContextId());
				if (!scope) {
					this.sendErrorResponse(response, 0, 'No debug context to evaluate {expression} in', { expression });
					return;
				}
				response.body = { result: await this.assignExpression(scope, expression, value) };
				this.sendResponse(response);
			} catch (e) {
				this.sendErrorResponse(response, 0, 'Could not set {expression}: {message}', { expression, message: AurixTCFDebugSession.formatError(e) });
			}
			return;
		}

		// Fall back to default behavior for unknown custom requests.
		await super.customRequestAsync(command, response, args, request);
	}
}
