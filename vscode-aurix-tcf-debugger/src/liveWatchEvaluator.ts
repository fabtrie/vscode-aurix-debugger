/*
 * Live Watch evaluation for the debug adapter.
 *
 * Evaluates the Live Watch expressions while the target runs and returns them as value trees
 * (a one-line preview per node, children only for nodes the view has expanded).
 *
 * Refreshing has to be cheap: the TCF expression, its type and symbol information and struct
 * layouts do not change while the program runs, so they are looked up once and cached. A steady
 * state refresh then costs one memory read (or one Expressions.evaluate) per expression.
 */

import { TCFClient, decodeEvaluatedValue } from './tcfclient';
import { CreateExpressionsCommand, DisposeExpressionsCommand, EvaluateExpressionsCommand, EvaluateResult } from './tcf/expressions';
import { GetMemoryCommand } from './tcf/memory';
import { GetChildrenSymbolsCommand, GetContextSymbolsCommand, TCFSymbolContextData, TCFTypeClass } from './tcf/symbols';
import { SimpleCommand, TCFPartialResultError } from './tcf/tcfutils';
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
};

export type LiveWatchRequestItem = {
	expression: string;
	/** Expressions of the expanded nodes below (and including) this root. */
	expanded?: string[];
};

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

/** C expression of a child node below a parent expression. */
export function liveWatchChildExpression(parentExpr: string, childName: string): string {
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
	private readonly helper: DefaultVariableHelper;

	constructor(private readonly client: TCFClient, private readonly log: (message: string) => void) {
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
		this.symbolCache.clear();
	}

	async evaluate(ctx: string, items: LiveWatchRequestItem[]): Promise<LiveWatchRootResult[]> {
		// TCF commands are pipelined, so evaluating all expressions at once saves a round trip each.
		return Promise.all(items.map(async (item) => {
			try {
				const expanded = new Set(item.expanded ?? []);
				const node = await this.evaluateRoot(ctx, item.expression, item.expression, expanded);
				return { ...node, available: true };
			} catch (e) {
				return {
					name: item.expression,
					expression: item.expression,
					value: '',
					hasChildren: false,
					available: false,
					error: formatError(e),
				};
			}
		}));
	}

	private async evaluateRoot(ctx: string, name: string, expression: string, expanded: Set<string>): Promise<LiveWatchNode> {
		const key = `${ctx}\n${expression}`;
		let entry = this.roots.get(key);
		let variable: ClientVariable | undefined;
		if (entry) {
			try {
				variable = await this.createRootVariable(ctx, entry);
			} catch {
				// The cached expression may have become invalid (e.g. a reset); create it again.
				this.dropRoot(key);
				entry = undefined;
			}
		}
		if (!entry) {
			entry = await this.createRoot(ctx, expression);
			this.roots.set(key, entry);
			this.evictRoots();
			variable = await this.createRootVariable(ctx, entry);
		}
		if (!variable) {
			throw new Error(`'${expression}' has no displayable value`);
		}
		return this.toNode(ctx, variable, name, expression, expanded);
	}

	private async createRoot(ctx: string, expression: string): Promise<RootEntry> {
		const exprId = await this.client.sendCommand(new CreateExpressionsCommand(ctx, null, expression));
		try {
			const evaluated = await this.evaluateExpression(exprId);
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

	private async createRootVariable(ctx: string, entry: RootEntry): Promise<ClientVariable | undefined> {
		const value = new BufferValueProvider(await this.readValue(ctx, entry), entry.bigEndian);
		const nameProvider = new FixedNameProvider(entry.symbol?.Name ?? '');
		const variable = this.helper.createVariable(entry.type, entry.symbol, nameProvider, value);
		if (!variable && !entry.symbol && entry.type.TypeClass === TCFTypeClass.composite) {
			// A struct without a symbol (e.g. "a.b"): take the field layout from its type.
			return new CompositeVariable(entry.type, entry.type, this.helper, nameProvider, value);
		}
		return variable;
	}

	private async readValue(ctx: string, entry: RootEntry): Promise<Buffer | undefined> {
		const symbol = entry.symbol;
		if (symbol?.Value) {
			return Buffer.from(symbol.Value, 'base64');
		}
		if (symbol?.Address && symbol.Size) {
			const r = await this.client.sendCommand(new GetMemoryCommand(ctx, symbol.Address, 1, symbol.Size));
			return Buffer.from(r.base64, 'base64');
		}
		const evaluated = await this.evaluateExpression(entry.exprId);
		return decodeEvaluatedValue(evaluated.value);
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

function formatError(e: unknown): string {
	if (e instanceof Error) {
		return e.message;
	}
	const format = (Array.isArray(e) ? e[0] : e as any)?.Format;
	return typeof format === 'string' ? format : String(e);
}
