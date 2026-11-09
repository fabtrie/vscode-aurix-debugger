/*
 * Live Watch evaluation for the debug adapter.
 *
 * Evaluates the Live Watch expressions while the target runs and returns them as value trees
 * (a one-line preview per node, children only for nodes the view has expanded).
 *
 * Refreshing has to be cheap: the TCF expression, its type and symbol information and struct
 * layouts do not change while the program runs, so they are looked up once and cached. A steady
 * state refresh then costs one memory read (or one Expressions.evaluate) per expression.
 *
 * A view's periodic refresh goes further: its roots' memory regions form the view's live set on
 * the server (AurixTarget.setLiveSet, sent again only when the regions or the interval change),
 * and one AurixTarget.getLiveValues reads all of them. The server decides per region where the
 * value comes from (sampled by the probe, or the same read Memory.get does); this does not know or
 * ask which probe is connected. An older server without live sets gets one Memory.get per root.
 */

import { CoreRef, parseCoreRef } from './breakpointCondition';
import { TCFClient, decodeEvaluatedValue } from './tcfclient';
import { ClearLiveSetAurixTargetCommand, GetLiveValuesAurixTargetCommand, LiveRegion, SetLiveSetAurixTargetCommand } from './tcf/aurixtarget';
import { TCFErrorCodes } from './tcf/error';
import { CreateExpressionsCommand, DisposeExpressionsCommand, EvaluateExpressionsCommand, EvaluateResult, GetContextExpressionsCommand } from './tcf/expressions';
import { GetMemoryCommand } from './tcf/memory';
import { FindByAddrSymbolsCommand, GetChildrenSymbolsCommand, GetContextSymbolsCommand, TCFSymbolContextData, TCFTypeClass } from './tcf/symbols';
import { ServiceNotSupportedError, SimpleCommand, TCFPartialResultError } from './tcf/tcfutils';
import { ArrayVariable } from './variables/array';
import { CompositeVariable } from './variables/composite';
import { DefaultVariableHelper } from './variables/helper';
import { PointerVariable } from './variables/pointer';
import { SYM_FLAG_BIG_ENDIAN } from './variables/symbol';
import { ClientVariable, FixedNameProvider, RawValueProvider } from './variables/types';

export type LiveWatchNode = {
	name: string;
	/** C expression of this node, used to assign a value. Empty for the "more elements" marker. */
	expression: string;
	value: string;
	hasChildren: boolean;
	/** Only set for expanded nodes. */
	children?: LiveWatchNode[];
};

export type LiveWatchRootResult = LiveWatchNode & {
	available: boolean;
	error?: string;
	/**
	 * The read was not done because the target started running (the server drops reads queued
	 * before a resume, step or reset). The view keeps the value it has.
	 */
	stale?: boolean;
};

/** A read refused because the context runs, or dropped for a resume queued after it. */
export function isRunningError(e: unknown): boolean {
	return formatError(e).includes('Execution context is running');
}

export type LiveWatchRequestItem = {
	expression: string;
	/** Expressions of the expanded nodes below (and including) this root. */
	expanded?: string[];
	/** Shows only these bits of an integer value (a peripheral register field). */
	bitOffset?: number;
	bitWidth?: number;
	/** Shows an integer value as zero-padded hex (a peripheral register). */
	hex?: boolean;
	/**
	 * A raw memory region (a peripheral register): `size` bytes at `address` are read and shown as
	 * hex (or the bits `bitOffset`/`bitWidth` of them) instead of evaluating `expression`, which
	 * then only names the result.
	 */
	address?: number;
	size?: number;
};

/**
 * Set for a view's periodic refresh: the roots' regions are the live set of `viewId`, sampled
 * every `intervalMs`.
 */
export type LiveWatchLiveOptions = { viewId: string; intervalMs: number };

type ResolvedRoot = {
	ctx: string;
	key: string;
	entry: RootEntry;
	explicitCore: boolean;
	/** Found in the cache rather than created for this refresh. */
	cached: boolean;
};

/** A root after the first pass of a live refresh. */
type ResolvedItem =
	/** Read from memory: through the live set. */
	| { kind: 'region'; region: LiveRegion; build: (value: Buffer) => Promise<LiveWatchNode>; readFailed: () => void }
	/** No memory region (a constant, or a value only Expressions.evaluate gives): evaluated as always. */
	| { kind: 'other' }
	| { kind: 'error'; error: unknown };

type RootEntry = {
	exprId: string;
	type: TCFSymbolContextData;
	symbol: TCFSymbolContextData | undefined;
	bigEndian: boolean;
};

const MAX_CHILDREN = 1000;
const MAX_PREVIEW_LENGTH = 100;
const MAX_PREVIEW_DEPTH = 2;
const MAX_CACHED_ROOTS = 500;

class BufferValueProvider implements RawValueProvider {
	constructor(private readonly buffer: Buffer | undefined, private readonly bigEndian: boolean) {
	}

	async value(): Promise<Buffer | undefined> {
		return this.buffer;
	}

	isBigEndian(): boolean {
		return this.bigEndian;
	}
}

/**
 * A Live Watch expression with an optional core: `expr@N` is evaluated on core N (e.g. a
 * variable in core-local memory, which every core has its own copy of), `expr@hsm` on the HSM,
 * `expr@scr` on the standby controller (SCR), as for a breakpoint.
 */
export function splitLiveWatchCore(expression: string): { expression: string; core?: CoreRef } {
	const m = /^(.*\S)\s*@\s*(\d+|hsm|scr)$/i.exec(expression.trim());
	if (!m) {
		return { expression: expression.trim() };
	}
	return { expression: m[1], core: parseCoreRef(m[2]) };
}

/** C expression of a child node below a parent expression; a core (`@N`, `@hsm`, `@scr`) carries over. */
export function liveWatchChildExpression(parentExpression: string, childName: string): string {
	const { expression: parentExpr, core } = splitLiveWatchCore(parentExpression);
	const child = childExpression(parentExpr, childName);
	return core === undefined ? child : `${child}@${core}`;
}

function childExpression(parentExpr: string, childName: string): string {
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

export class LiveWatchEvaluator {
	/** Keyed by context + "\n" + expression; insertion order is used to evict the oldest entries. */
	private readonly roots = new Map<string, RootEntry>();
	private readonly symbolCache = new Map<string, Promise<unknown>>();
	/**
	 * Roots being created, by key: several Live Watch instances can ask for the same new expression
	 * at once, and each creating its own would leak all but one of them in the agent.
	 */
	private readonly creating = new Map<string, Promise<RootEntry>>();
	private readonly helper: DefaultVariableHelper;
	/**
	 * The live set each view has on the server, by view id: its signature (interval and regions),
	 * or undefined when the server may have one that is not known to match.
	 */
	private readonly liveSets = new Map<string, string | undefined>();
	/** The server does not know AurixTarget.setLiveSet (an older one): every read is a Memory.get. */
	private liveSetUnsupported = false;

	/** `coreContext`: the context of core N, the HSM or the SCR, for `expr@N`, `expr@hsm` and `expr@scr`. */
	constructor(
		private readonly client: TCFClient,
		private readonly log: (message: string) => void,
		private readonly coreContext: (core: CoreRef) => Promise<string | undefined> = async () => undefined,
	) {
		const self = this;
		this.helper = new class extends DefaultVariableHelper {
			log(message: string): void {
				self.log(message);
			}
			sendCommand<T>(c: SimpleCommand<T>): Promise<T> {
				return self.sendCached(c);
			}
		}();
	}

	/** Forgets all cached expressions and symbols, e.g. after a reset or when the program changed. */
	clear(): void {
		for (const entry of this.roots.values()) {
			void this.client.sendCommand(new DisposeExpressionsCommand(entry.exprId)).catch(() => undefined);
		}
		this.roots.clear();
		this.creating.clear();
		this.symbolCache.clear();
	}

	/**
	 * Evaluates the roots, one result per item in order. With `live` (a view's periodic refresh)
	 * their memory is read through the view's live set, with one request for all of them.
	 */
	async evaluate(ctx: string, items: LiveWatchRequestItem[], live?: LiveWatchLiveOptions): Promise<LiveWatchRootResult[]> {
		if (!live || this.liveSetUnsupported) {
			// TCF commands are pipelined, so evaluating all expressions at once saves a round trip each.
			return Promise.all(items.map((item) => this.evaluateItem(ctx, item)));
		}
		// Pass 1: the expressions and symbols, and so the regions (all cached after the first refresh).
		const resolved = await Promise.all(items.map(async (item): Promise<ResolvedItem> => {
			try {
				return await this.resolveItem(ctx, item);
			} catch (error) {
				return { kind: 'error', error };
			}
		}));
		// Pass 2: every region in one request.
		const regions: LiveRegion[] = [];
		const regionIndex = resolved.map((r) => (r.kind === 'region' ? regions.push(r.region) - 1 : -1));
		const values = await this.readLive(live, regions);
		return Promise.all(resolved.map(async (r, index): Promise<LiveWatchRootResult> => {
			const item = items[index];
			if (r.kind === 'error') {
				return errorResult(item, r.error);
			}
			if (r.kind === 'other' || !values) {
				// No region, or no live set to read it with: as a one-off refresh does.
				return this.evaluateItem(ctx, item);
			}
			try {
				const value = values[regionIndex[index]];
				if (value instanceof Error) {
					if (!isRunningError(value)) {
						r.readFailed();
					}
					throw value;
				}
				return { ...(await r.build(value)), available: true };
			} catch (e) {
				return errorResult(item, e);
			}
		}));
	}

	/**
	 * The view no longer refreshes periodically (hidden, paused, resting while halted, closed):
	 * the server drops its live set and stops sampling for it. The next live refresh sets it again.
	 */
	async stopLive(viewId: string): Promise<void> {
		if (!this.liveSets.has(viewId)) {
			return;
		}
		this.liveSets.delete(viewId);
		try {
			await this.client.sendCommand(new ClearLiveSetAurixTargetCommand(viewId));
		} catch (e) {
			this.log(`AurixTarget.clearLiveSet ${viewId} failed: ${formatError(e)}`);
		}
	}

	/** One root, read on its own (a one-off refresh, or no live set). */
	private async evaluateItem(ctx: string, item: LiveWatchRequestItem): Promise<LiveWatchRootResult> {
		try {
			const region = rawRegion(ctx, item);
			if (region) {
				return { ...rawNode(item, await this.readRegion(region)), available: true };
			}
			const expanded = new Set(item.expanded ?? []);
			const node = await this.evaluateRoot(ctx, item.expression, item.expression, expanded, item);
			return { ...node, available: true };
		} catch (e) {
			return errorResult(item, e);
		}
	}

	/** First pass of a live refresh: where the root's value is, without reading it. */
	private async resolveItem(ctx: string, item: LiveWatchRequestItem): Promise<ResolvedItem> {
		const raw = rawRegion(ctx, item);
		if (raw) {
			return { kind: 'region', region: raw, build: async (value) => rawNode(item, value), readFailed: () => undefined };
		}
		const root = await this.resolveRoot(ctx, item.expression);
		const region = this.rootRegion(root.ctx, root.entry, root.explicitCore);
		if (!region) {
			return { kind: 'other' };
		}
		const expanded = new Set(item.expanded ?? []);
		return {
			kind: 'region',
			region,
			build: (value) => this.buildRoot(root.ctx, root.entry, value, item.expression, item.expression, expanded, item),
			// As for a one-off read: the cached expression may have become invalid (e.g. a reset),
			// so the next refresh creates it again.
			readFailed: () => {
				if (root.cached) {
					this.dropRoot(root.key);
				}
			},
		};
	}

	/**
	 * Reads the regions through the live set of the view, setting it first if it changed. One
	 * value or error per region, or undefined when there is no live set to read with: an older
	 * server (remembered for the session), or setting or reading the set failed this time.
	 */
	private async readLive(live: LiveWatchLiveOptions, regions: LiveRegion[]): Promise<Array<Buffer | Error> | undefined> {
		// A register and its fields, or a variable watched twice, are one region of the set.
		const unique: LiveRegion[] = [];
		const uniqueIndex = new Map<string, number>();
		const slots = regions.map((r) => {
			const key = `${r.ctx}\n${r.addr}\n${r.size}`;
			let i = uniqueIndex.get(key);
			if (i === undefined) {
				i = unique.push({ ctx: r.ctx, addr: r.addr, size: r.size }) - 1;
				uniqueIndex.set(key, i);
			}
			return i;
		});
		if (unique.length === 0) {
			// Nothing in memory to sample (only constants and expressions without an address).
			await this.stopLive(live.viewId);
			return [];
		}
		const periodUs = Math.max(1, Math.round(live.intervalMs * 1000));
		const signature = JSON.stringify([periodUs, unique]);
		try {
			if (this.liveSets.get(live.viewId) !== signature) {
				// Until the server confirms, it may or may not have the set: stopLive still clears it.
				this.liveSets.set(live.viewId, undefined);
				await this.client.sendCommand(new SetLiveSetAurixTargetCommand(live.viewId, unique, periodUs));
				this.liveSets.set(live.viewId, signature);
			}
			const values = await this.client.sendCommand(new GetLiveValuesAurixTargetCommand(live.viewId));
			if (values.length !== unique.length) {
				throw new Error(`AurixTarget.getLiveValues returned ${values.length} values for ${unique.length} regions`);
			}
			const decoded = values.map((v) => v.error !== undefined
				? new Error(v.error)
				: v.data !== undefined ? Buffer.from(v.data, 'base64') : new Error('No value'));
			return slots.map((i) => decoded[i]);
		} catch (e) {
			if (isUnknownCommand(e)) {
				this.liveSets.delete(live.viewId);
				this.liveSetUnsupported = true;
				this.log('The debug server has no live sets (AurixTarget.setLiveSet): Live Watch reads every value with Memory.get');
				return undefined;
			}
			if (isRunningError(e)) {
				// Dropped for a resume queued after it, as dropped Memory.gets are: every root keeps
				// what it shows, and the set stays as it is.
				const running = e instanceof Error ? e : new Error(formatError(e));
				return regions.map(() => running);
			}
			// Set it again on the next refresh, in case the server lost it.
			this.liveSets.set(live.viewId, undefined);
			if (!isRunningError(e)) {
				this.log(`Live set ${live.viewId} failed, reading every value with Memory.get: ${formatError(e)}`);
			}
			return undefined;
		}
	}

	private async evaluateRoot(ctx: string, name: string, expression: string, expanded: Set<string>, format?: LiveWatchRequestItem): Promise<LiveWatchNode> {
		let root = await this.resolveRoot(ctx, expression);
		let value: Buffer | undefined;
		try {
			value = await this.readValue(root.ctx, root.entry, root.explicitCore);
		} catch (e) {
			if (!root.cached || isRunningError(e)) {
				// Running: not read (a resume came after it), but the expression is still good.
				throw e;
			}
			// The cached expression may have become invalid (e.g. a reset); create it again.
			this.dropRoot(root.key);
			root = await this.resolveRoot(ctx, expression);
			value = await this.readValue(root.ctx, root.entry, root.explicitCore);
		}
		return this.buildRoot(root.ctx, root.entry, value, name, expression, expanded, format);
	}

	/** The context to evaluate `expression` in (`expr@N`: core N's) and its cached or new root. */
	private async resolveRoot(ctx: string, expression: string): Promise<ResolvedRoot> {
		const { expression: text, core } = splitLiveWatchCore(expression);
		if (core !== undefined) {
			const coreCtx = await this.coreContext(core);
			if (!coreCtx) {
				throw new Error(typeof core === 'number' ? `No core ${core}` : `No ${core.toUpperCase()}`);
			}
			ctx = coreCtx;
		}
		const explicitCore = core !== undefined;
		const key = `${ctx}\n${expression}`;
		const cached = this.roots.get(key);
		if (cached) {
			return { ctx, key, entry: cached, explicitCore, cached: true };
		}
		return { ctx, key, entry: await this.createRootOnce(ctx, key, text), explicitCore, cached: false };
	}

	/** The node of a root, from its value. */
	private async buildRoot(ctx: string, entry: RootEntry, value: Buffer | undefined, name: string, expression: string, expanded: Set<string>, format?: LiveWatchRequestItem): Promise<LiveWatchNode> {
		const variable = this.createRootVariable(entry, value);
		if (!variable) {
			throw new Error(`'${expression}' has no displayable value`);
		}
		if (format && (format.hex || format.bitWidth !== undefined)) {
			return { name, expression, value: await formatBits(variable, format), hasChildren: false };
		}
		return this.toNode(ctx, variable, name, expression, expanded);
	}

	/** Creates a root, or joins the creation another request already started for the same key. */
	private createRootOnce(ctx: string, key: string, expression: string): Promise<RootEntry> {
		const existing = this.creating.get(key);
		if (existing) {
			return existing;
		}
		const pending: Promise<RootEntry> = this.createRoot(ctx, expression).then((entry) => {
			this.roots.set(key, entry);
			this.evictRoots();
			return entry;
		}).finally(() => {
			if (this.creating.get(key) === pending) {
				this.creating.delete(key);
			}
		});
		this.creating.set(key, pending);
		return pending;
	}

	private async createRoot(ctx: string, expression: string): Promise<RootEntry> {
		const exprId = await this.client.sendCommand(new CreateExpressionsCommand(ctx, null, expression));
		try {
			const evaluated = await this.evaluateExpression(exprId).catch(async (e) => {
				// Evaluating also reads the value, which fails for memory the agent cannot read (e.g. HSM RAM).
				// The expression context still has the type and symbol, and reading the memory later gives the real error.
				const context = await this.client.sendCommand(new GetContextExpressionsCommand(exprId)).catch(() => null);
				if (!context?.Type) {
					throw e;
				}
				return { properties: { Type: context.Type, Symbol: context.SymbolID } } as EvaluateResult;
			});
			const typeId = evaluated.properties?.Type;
			if (!typeId) {
				throw new Error(`No type information for '${expression}'`);
			}
			const type = await this.sendCached(new GetContextSymbolsCommand(typeId));
			const symbolId = evaluated.properties?.Symbol;
			const symbol = symbolId ? await this.sendCached(new GetContextSymbolsCommand(symbolId)) : undefined;
			const bigEndian = symbol?.Flags !== undefined
				? (symbol.Flags & SYM_FLAG_BIG_ENDIAN) !== 0
				: evaluated.properties?.BigEndian ?? false;
			return { exprId, type, symbol, bigEndian };
		} catch (e) {
			void this.client.sendCommand(new DisposeExpressionsCommand(exprId)).catch(() => undefined);
			throw e;
		}
	}

	private createRootVariable(entry: RootEntry, buffer: Buffer | undefined): ClientVariable | undefined {
		const value = new BufferValueProvider(buffer, entry.bigEndian);
		const nameProvider = new FixedNameProvider(entry.symbol?.Name ?? '');
		const variable = this.helper.createVariable(entry.type, entry.symbol, nameProvider, value);
		if (!variable && !entry.symbol && entry.type.TypeClass === TCFTypeClass.composite) {
			// A struct without a symbol (e.g. "a.b"): take the field layout from its type.
			return new CompositeVariable(entry.type, entry.type, this.helper, nameProvider, value);
		}
		return variable;
	}

	/** `explicitCore`: read with `ctx` (`expr@N`) even if the symbol belongs to another core. */
	private async readValue(ctx: string, entry: RootEntry, explicitCore: boolean): Promise<Buffer | undefined> {
		const symbol = entry.symbol;
		if (symbol?.Value) {
			return Buffer.from(symbol.Value, 'base64');
		}
		const region = this.rootRegion(ctx, entry, explicitCore);
		if (region) {
			return this.readRegion(region);
		}
		const evaluated = await this.evaluateExpression(entry.exprId);
		return decodeEvaluatedValue(evaluated.value);
	}

	/** Where a root's value is in memory; undefined for a constant or a value without an address. */
	private rootRegion(ctx: string, entry: RootEntry, explicitCore: boolean): LiveRegion | undefined {
		const symbol = entry.symbol;
		if (symbol?.Value || !symbol?.Address || !symbol.Size) {
			return undefined;
		}
		// A symbol owned by one core (e.g. an HSM variable, or one in the core-local memory of the
		// core that runs its ELF) is read with that core's memory view.
		const owner = !explicitCore && typeof symbol.OwnerID === 'string' && /\.C\d+$/.test(symbol.OwnerID) ? symbol.OwnerID : ctx;
		return { ctx: owner, addr: symbol.Address, size: symbol.Size };
	}

	private async readRegion(region: LiveRegion): Promise<Buffer> {
		const r = await this.client.sendCommand(new GetMemoryCommand(region.ctx, region.addr, 1, region.size));
		return Buffer.from(r.base64, 'base64');
	}

	private async evaluateExpression(exprId: string): Promise<EvaluateResult> {
		try {
			return await this.client.sendCommand(new EvaluateExpressionsCommand(exprId));
		} catch (e) {
			if (e instanceof TCFPartialResultError && (e as TCFPartialResultError<EvaluateResult>).result.properties !== null) {
				return (e as TCFPartialResultError<EvaluateResult>).result;
			}
			throw e;
		}
	}

	private async toNode(ctx: string, v: ClientVariable, name: string, expression: string, expanded: Set<string>): Promise<LiveWatchNode> {
		const isPointer = v instanceof PointerVariable && (await this.canDereference(v));
		const isAggregate = v instanceof CompositeVariable || v instanceof ArrayVariable;
		const children = isAggregate ? await v.getChildren() : [];
		const node: LiveWatchNode = {
			name,
			expression,
			value: await this.preview(v, 0),
			hasChildren: isPointer || children.length > 0,
		};
		if (!node.hasChildren || !expanded.has(expression)) {
			return node;
		}
		if (isPointer) {
			const target = liveWatchChildExpression(expression, '*');
			try {
				node.children = [await this.evaluateRoot(ctx, `*${name}`, target, expanded)];
			} catch (e) {
				node.children = [{ name: `*${name}`, expression: target, value: `<${formatError(e)}>`, hasChildren: false }];
			}
			return node;
		}
		node.children = await Promise.all(children.slice(0, MAX_CHILDREN).map((c) => {
			const childName = c.name() ?? '?';
			return this.toNode(ctx, c, childName, liveWatchChildExpression(expression, childName), expanded);
		}));
		if (children.length > MAX_CHILDREN) {
			node.children.push({ name: '…', expression: '', value: `${children.length - MAX_CHILDREN} more`, hasChildren: false });
		}
		return node;
	}

	private async canDereference(v: PointerVariable): Promise<boolean> {
		if (!v.type.BaseTypeID) {
			return false;
		}
		try {
			const base = await this.sendCached(new GetContextSymbolsCommand(v.type.BaseTypeID));
			return base.TypeClass !== TCFTypeClass.function && (base.Size ?? 0) > 0;
		} catch {
			return false;
		}
	}

	/** One-line value like the Watch view shows it: `{a: 1, b: {…}}`, `[1, 2, 3, …]`. */
	private async preview(v: ClientVariable, depth: number): Promise<string> {
		if (v instanceof CompositeVariable || v instanceof ArrayVariable) {
			const isArray = v instanceof ArrayVariable;
			if (isArray) {
				const text = await v.displayValue();
				if (text !== undefined && text !== '[...]') {
					return text; // char array shown as a string
				}
			}
			const [open, close] = isArray ? ['[', ']'] : ['{', '}'];
			if (depth >= MAX_PREVIEW_DEPTH) {
				return `${open}…${close}`;
			}
			const children = await v.getChildren();
			let text = '';
			for (let i = 0; i < children.length; i++) {
				if (text.length >= MAX_PREVIEW_LENGTH) {
					text += ', …';
					break;
				}
				const value = await this.preview(children[i], depth + 1);
				text += (i > 0 ? ', ' : '') + (isArray ? value : `${children[i].name()}: ${value}`);
			}
			return `${open}${text}${close}`;
		}
		return (await v.displayValue()) ?? '?';
	}

	/** Symbol information is static, so the Symbols service lookups used by the variables are cached. */
	private sendCached<T>(c: SimpleCommand<T>): Promise<T> {
		let key: string | undefined;
		if (c instanceof GetContextSymbolsCommand) {
			key = `context:${c.contextID}`;
		} else if (c instanceof GetChildrenSymbolsCommand) {
			key = `children:${c.contextID}`;
		} else if (c instanceof FindByAddrSymbolsCommand) {
			// A function pointer's target, looked up on every refresh otherwise.
			key = `addr:${c.contextID}:${c.address}`;
		}
		if (!key) {
			return this.client.sendCommand(c);
		}
		let cached = this.symbolCache.get(key);
		if (!cached) {
			const cacheKey = key;
			cached = this.client.sendCommand(c).catch((e) => {
				this.symbolCache.delete(cacheKey);
				throw e;
			});
			this.symbolCache.set(key, cached);
		}
		// The variable classes modify these objects (e.g. append "*" to a pointer type name).
		return cached.then((value) => (Array.isArray(value) ? [...value] : value && typeof value === 'object' ? { ...value } : value) as T);
	}

	private dropRoot(key: string): void {
		const entry = this.roots.get(key);
		if (entry) {
			this.roots.delete(key);
			void this.client.sendCommand(new DisposeExpressionsCommand(entry.exprId)).catch(() => undefined);
		}
	}

	private evictRoots(): void {
		for (const key of this.roots.keys()) {
			if (this.roots.size <= MAX_CACHED_ROOTS) {
				break;
			}
			this.dropRoot(key);
		}
	}
}

/** The region of a raw item (a peripheral register), read with `ctx`. */
function rawRegion(ctx: string, item: LiveWatchRequestItem): LiveRegion | undefined {
	return item.address !== undefined && item.size !== undefined && item.size > 0
		? { ctx, addr: item.address, size: item.size }
		: undefined;
}

/** A raw item's node: the register as hex, or the field of it. TriCore memory is little-endian. */
function rawNode(item: LiveWatchRequestItem, value: Buffer): LiveWatchNode {
	return { name: item.expression, expression: item.expression, value: formatRawBits(value, false, item), hasChildren: false };
}

function errorResult(item: LiveWatchRequestItem, e: unknown): LiveWatchRootResult {
	return {
		name: item.expression,
		expression: item.expression,
		value: '',
		hasChildren: false,
		available: false,
		error: formatError(e),
		stale: isRunningError(e),
	};
}

/** The server does not know the command (an older one), or not the AurixTarget service at all. */
function isUnknownCommand(e: unknown): boolean {
	return e instanceof ServiceNotSupportedError || (e as any)?.Code === TCFErrorCodes.INV_COMMAND;
}

/** Register value as hex, or a field of it: hex for multi-bit fields, 0/1 for single bits. */
async function formatBits(v: ClientVariable, format: LiveWatchRequestItem): Promise<string> {
	return formatRawBits(await v.value(), v.isBigEndian(), format);
}

function formatRawBits(raw: Buffer | undefined, bigEndian: boolean, format: LiveWatchRequestItem): string {
	if (!raw || raw.length === 0) {
		throw new Error('no value');
	}
	const bytes = bigEndian ? raw : Buffer.from(raw).reverse();
	let value = BigInt(`0x${bytes.toString('hex')}`);
	let bits = raw.length * 8;
	if (format.bitWidth !== undefined) {
		value = (value >> BigInt(format.bitOffset ?? 0)) & ((1n << BigInt(format.bitWidth)) - 1n);
		bits = format.bitWidth;
	}
	if (bits === 1) {
		return value.toString();
	}
	return `0x${value.toString(16).padStart(Math.ceil(bits / 4), '0')}`;
}

function formatError(e: unknown): string {
	if (e instanceof TCFPartialResultError && e.cause !== undefined) {
		// "Partial result" says nothing; the agent's error is the cause.
		return formatError(e.cause);
	}
	if (e instanceof Error) {
		return e.message;
	}
	const format = (Array.isArray(e) ? e[0] : e as any)?.Format;
	return typeof format === 'string' ? format : String(e);
}
