/*
 * SVD file -> peripheral definitions for the XPeripherals view (mcu-debug.peripheral-viewer).
 *
 * Given an SVD file, the view parses it with xml2js in the extension host on the first session
 * after VS Code started: for the TC3xx files (~60 MB) that blocks the extension host - and so the
 * debug adapter, which runs in it - for about 6 seconds. The view also takes the peripherals from
 * a command instead (launch configuration "peripherals": "command:..."), as the options its
 * PeripheralNode is built from. This module turns an SVD file into these options, the same way
 * the view's own SVD parser (src/svd-parser.ts, version 1.6.5) does, so they can be converted
 * once, off the extension host, and read back as JSON.
 *
 * No vscode import: also used by the conversion worker.
 */

/** AccessType of the peripheral viewer. */
const enum Access {
	readOnly = 1,
	readWrite = 2,
	writeOnly = 3,
}

export type EnumerationMap = { [value: number]: { name: string; description: string; value: number } };

export type FieldOptions = {
	name: string;
	description: string;
	offset: number;
	width: number;
	enumeration?: EnumerationMap;
	accessType?: number;
	readAction?: number;
};

export type RegisterOptions = {
	name: string;
	description?: string;
	addressOffset: number;
	accessType?: number;
	readAction?: number;
	size?: number;
	resetValue?: number;
	fields?: FieldOptions[];
};

export type ClusterOptions = {
	name: string;
	description?: string;
	addressOffset: number;
	accessType?: number;
	size?: number;
	resetValue?: number;
	registers?: RegisterOptions[];
	clusters?: ClusterOptions[];
};

export type PeripheralOptions = {
	name: string;
	baseAddress: number;
	totalLength: number;
	description: string;
	groupName?: string;
	accessType?: number;
	size?: number;
	resetValue?: number;
	registers?: RegisterOptions[];
	clusters?: ClusterOptions[];
};

type ParentNode = { children: { offset: number }[] };
type NodeClass = { addChild(this: ParentNode, child: unknown): unknown };

/**
 * The peripherals as returned to the view.
 *
 * Peripheral viewer 1.6.5 adds each register and cluster it builds from options twice: the node's
 * constructor adds itself to its parent, and the parent adds it again. Besides showing everything
 * twice, that makes building slow: the parent sorts its children after each add, and the pairs
 * of equal children make each sort a full one (TC3xx: 6 s). The view builds the peripherals with
 * `options.map(...)`, so while this list's map runs (synchronously), the view's peripheral and
 * cluster nodes add each child once and sort their children once at the end, with the view's own
 * order (TC3xx: 0.25 s, the same tree). Duplicates still there afterwards (e.g. if the classes
 * could not be found) are removed before the view computes the address ranges to read.
 */
export class PeripheralOptionsList extends Array<PeripheralOptions> {
	static get [Symbol.species](): ArrayConstructor {
		return Array;
	}

	override map<U>(callbackfn: (value: PeripheralOptions, index: number, array: PeripheralOptions[]) => U, thisArg?: unknown): U[] {
		const build = (value: PeripheralOptions, index: number) => callbackfn.call(thisArg, value, index, this);
		const restore = addChildrenOnce(build);
		let nodes: U[];
		try {
			nodes = Array.from(this, build);
		} finally {
			restore();
		}
		for (const node of nodes) {
			removeDuplicateChildren(node);
		}
		return nodes;
	}
}

/**
 * Until the returned function is called, the view's peripheral and cluster nodes add each child
 * once, unsorted; the returned function puts their addChild back and sorts the children as
 * addChild does.
 */
function addChildrenOnce(build: (value: PeripheralOptions, index: number) => unknown): () => void {
	let probe: { children?: unknown[] } | undefined;
	try {
		// A throwaway peripheral with a cluster, to get the classes the view builds with.
		probe = build({ name: '', baseAddress: 0, totalLength: 0, description: '', clusters: [{ name: '', addressOffset: 0 }] }, 0) as typeof probe;
	} catch {
		return () => undefined;
	}
	const added = new WeakSet<object>();
	const parents = new Set<ParentNode>();
	const patched: [NodeClass, NodeClass['addChild']][] = [];
	for (const node of [probe, probe?.children?.[0]]) {
		const proto = node && typeof node === 'object' ? Object.getPrototypeOf(node) as NodeClass : undefined;
		if (!proto || !Object.prototype.hasOwnProperty.call(proto, 'addChild') || typeof proto.addChild !== 'function'
			|| patched.some(([p]) => p === proto)) {
			continue;
		}
		const original = proto.addChild;
		proto.addChild = function (child: unknown) {
			if (!Array.isArray(this.children) || !child || typeof child !== 'object') {
				return original.call(this, child);
			}
			if (!added.has(child)) {
				added.add(child);
				this.children.push(child as { offset: number });
				parents.add(this);
			}
		};
		patched.push([proto, original]);
	}
	return () => {
		for (const [proto, original] of patched) {
			proto.addChild = original;
		}
		for (const parent of parents) {
			// The order of the view's addChild.
			parent.children.sort((c1, c2) => (c1.offset > c2.offset ? 1 : -1));
		}
	};
}

function removeDuplicateChildren(node: unknown): void {
	const children = (node as { children?: unknown } | undefined)?.children;
	if (!Array.isArray(children)) {
		return;
	}
	const unique = Array.from(new Set(children));
	if (unique.length !== children.length) {
		children.length = 0;
		for (const child of unique) {
			children.push(child);
		}
	}
	for (const child of unique) {
		removeDuplicateChildren(child);
	}
}

// --- XML -------------------------------------------------------------------------------------

/**
 * An element as xml2js (default options) returns it, which is what the view's parser works on:
 * child elements as arrays by tag name, attributes in `$`, text in `_` - and an element with
 * text only (no attributes, no children) as its text.
 */
type XmlElement = { [tag: string]: any };

const ENTITIES: Record<string, string> = { lt: '<', gt: '>', amp: '&', quot: '"', apos: '\'' };

function decodeEntities(text: string): string {
	if (!text.includes('&')) {
		return text;
	}
	return text.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);/g, (whole, ref: string) => {
		if (ref[0] === '#') {
			const code = ref[1] === 'x' ? parseInt(ref.substring(2), 16) : parseInt(ref.substring(1), 10);
			return Number.isFinite(code) ? String.fromCodePoint(code) : whole;
		}
		return ENTITIES[ref] ?? whole;
	});
}

type Frame = { name: string; obj: XmlElement; text: string; cdata: boolean };

/** Parses an XML document into the xml2js form: `{ <root tag>: element }`. */
export function parseXml(xml: string): XmlElement {
	const stack: Frame[] = [];
	let root: XmlElement | undefined;
	const close = () => {
		const frame = stack.pop()!;
		let value: any = frame.obj;
		let emptyText = '';
		if (/^\s*$/.test(frame.text) && !frame.cdata) {
			emptyText = frame.text;
		} else {
			value._ = frame.text;
		}
		const keys = Object.keys(value);
		if (keys.length === 0) {
			value = emptyText;
		} else if (keys.length === 1 && keys[0] === '_') {
			value = value._;
		}
		const parent = stack[stack.length - 1];
		if (parent) {
			(parent.obj[frame.name] ??= []).push(value);
		} else {
			root = { [frame.name]: value };
		}
	};
	let i = 0;
	while (i < xml.length) {
		const lt = xml.indexOf('<', i);
		const textEnd = lt < 0 ? xml.length : lt;
		if (textEnd > i && stack.length > 0) {
			stack[stack.length - 1].text += decodeEntities(xml.substring(i, textEnd));
		}
		if (lt < 0) {
			break;
		}
		if (xml.startsWith('<!--', lt)) {
			i = endOf(xml, '-->', lt + 4);
			continue;
		}
		if (xml.startsWith('<![CDATA[', lt)) {
			const end = xml.indexOf(']]>', lt + 9);
			if (end < 0) {
				throw new Error('XML: unterminated CDATA section');
			}
			const top = stack[stack.length - 1];
			if (top) {
				top.text += xml.substring(lt + 9, end);
				top.cdata = true;
			}
			i = end + 3;
			continue;
		}
		if (xml[lt + 1] === '?') {
			i = endOf(xml, '?>', lt + 2);
			continue;
		}
		if (xml[lt + 1] === '!') {
			// DOCTYPE; an internal subset ([...]) is skipped as a whole.
			const bracket = xml.indexOf('[', lt);
			const gt = xml.indexOf('>', lt);
			i = bracket >= 0 && bracket < gt ? endOf(xml, ']>', bracket) : endOf(xml, '>', lt);
			continue;
		}
		const gt = tagEnd(xml, lt);
		if (xml[lt + 1] === '/') {
			const name = xml.substring(lt + 2, gt).trim();
			if (stack[stack.length - 1]?.name !== name) {
				throw new Error(`XML: unexpected </${name}> at offset ${lt}`);
			}
			close();
			i = gt + 1;
			continue;
		}
		const selfClosing = xml[gt - 1] === '/';
		const body = xml.substring(lt + 1, selfClosing ? gt - 1 : gt);
		const nameEnd = body.search(/[\s]/);
		const name = nameEnd < 0 ? body : body.substring(0, nameEnd);
		const obj: XmlElement = {};
		if (nameEnd >= 0) {
			const attrs: Record<string, string> = {};
			let any = false;
			for (const m of body.substring(nameEnd).matchAll(/([^\s=]+)\s*=\s*(?:"([^"]*)"|'([^']*)')/g)) {
				attrs[m[1]] = decodeEntities(m[2] ?? m[3] ?? '');
				any = true;
			}
			if (any) {
				obj.$ = attrs;
			}
		}
		stack.push({ name, obj, text: '', cdata: false });
		if (selfClosing) {
			close();
		}
		i = gt + 1;
	}
	if (stack.length > 0 || !root) {
		throw new Error(stack.length > 0 ? `XML: <${stack[stack.length - 1].name}> is not closed` : 'XML: no root element');
	}
	return root;
}

function endOf(xml: string, terminator: string, from: number): number {
	const end = xml.indexOf(terminator, from);
	if (end < 0) {
		throw new Error(`XML: missing "${terminator}"`);
	}
	return end + terminator.length;
}

/** The ">" closing the tag at `lt`; one in an attribute value does not count. */
function tagEnd(xml: string, lt: number): number {
	let quote = '';
	for (let i = lt + 1; i < xml.length; i++) {
		const c = xml[i];
		if (quote) {
			if (c === quote) {
				quote = '';
			}
		} else if (c === '"' || c === '\'') {
			quote = c;
		} else if (c === '>') {
			return i;
		}
	}
	throw new Error(`XML: unterminated tag at offset ${lt}`);
}

// --- SVD ------------------------------------------------------------------------------------
// From here on a port of the peripheral viewer's SVDParser, building options instead of nodes.

function parseInteger(value: string | undefined): number | undefined {
	if (value) {
		value = value.toLowerCase();
		if (/^0b([01]+)$/i.test(value)) {
			return parseInt(value.substring(2), 2);
		} else if (/^0x([0-9a-f]+)$/i.test(value)) {
			return parseInt(value.substring(2), 16);
		} else if (/^[0-9]+/i.test(value)) {
			return parseInt(value, 10);
		} else if (/^#[0-1]+/i.test(value)) {
			return parseInt(value.substring(1), 2);
		}
	}
	return undefined;
}

function parseDimIndex(spec: string, count: number): string[] {
	if (spec.indexOf(',') !== -1) {
		const components = spec.split(',').map((c) => c.trim());
		if (components.length !== count) {
			throw new Error('dimIndex Element has invalid specification.');
		}
		return components;
	}
	if (/^([0-9]+)-([0-9]+)$/i.test(spec)) {
		const [start, end] = spec.split('-').map((p) => parseInteger(p));
		if (start === undefined || end === undefined) {
			return [];
		}
		if (start < 0 || end < 0 || end < start) {
			throw new Error('dimIndex Element Range Spec invalid.');
		}
		if (end - start + 1 < count) {
			throw new Error('dimIndex Element has invalid specification.');
		}
		return Array.from({ length: count }, (_, i) => `${start + i}`);
	}
	if (/^[a-zA-Z]-[a-zA-Z]$/.test(spec)) {
		const start = spec.charCodeAt(0);
		if (spec.charCodeAt(2) - start + 1 < count) {
			throw new Error('dimIndex Element has invalid specification.');
		}
		return Array.from({ length: count }, (_, i) => String.fromCharCode(start + i));
	}
	return [];
}

function accessTypeFromString(type: string): number {
	switch (type) {
		case 'write-only':
		case 'writeOnce':
			return Access.writeOnly;
		case 'read-write':
		case 'read-writeOnce':
			return Access.readWrite;
		default:
			return Access.readOnly;
	}
}

function readActionFromString(type: string): number | undefined {
	switch (type) {
		case 'clear':
			return 1;
		case 'set':
			return 2;
		case 'modify':
			return 3;
		case 'modifyExternal':
			return 4;
	}
	return undefined;
}

function cleanupDescription(input: string): string {
	return input.replace(/\r/g, '').replace(/\n\s*/g, ' ');
}

function dimIndices(e: XmlElement, kind: string): { count: number; increment: number; index: string[] } {
	const count = parseInteger(e.dim[0]);
	if (!count || count < 1) {
		throw new Error(`Unable to parse SVD file: ${kind} ${e.name[0]} has dim element, with no/invalid dimensions.`);
	}
	const increment = e.dimIncrement?.[0] ? parseInteger(e.dimIncrement[0]) ?? 0 : 0;
	if (!increment && count > 1) {
		throw new Error(`Unable to parse SVD file: ${kind} ${e.name[0]} has dim element, with no/invalid dimIncrement element.`);
	}
	const index = e.dimIndex ? parseDimIndex(e.dimIndex[0], count) : Array.from({ length: count }, (_, i) => `${i}`);
	return { count, increment, index };
}

class SvdConverter {
	/** Named enumerations, also by their path ("FIELD.enum", "REG.FIELD.enum", ...). */
	private readonly enumTypeValuesMap: Record<string, EnumerationMap> = {};
	private readonly peripheralRegisterMap: Record<string, XmlElement> = {};
	/** Fields whose enumeration is named later in the file. */
	private readonly deferred: { field: FieldOptions; derivedFrom: string }[] = [];

	convert(svd: XmlElement): PeripheralOptions[] {
		const device = svd.device;
		if (!device || typeof device !== 'object') {
			throw new Error('Unable to parse SVD file: no device element');
		}
		const defaults = { accessType: Access.readWrite as number, size: 32, resetValue: 0 };
		if (device.resetValue) {
			defaults.resetValue = parseInteger(device.resetValue[0]) ?? 0;
		}
		if (device.size) {
			defaults.size = parseInteger(device.size[0]) ?? 32;
		}
		if (device.access) {
			defaults.accessType = accessTypeFromString(device.access[0]);
		}
		if (!device.peripherals) {
			throw new Error('Unable to parse SVD file: no peripherals defined');
		}
		const peripheralMap: Record<string, XmlElement> = {};
		for (const element of device.peripherals[0].peripheral ?? []) {
			peripheralMap[element.name[0]] = element;
		}
		for (const key in peripheralMap) {
			const element = peripheralMap[key];
			if (element.$?.derivedFrom) {
				peripheralMap[key] = { ...peripheralMap[element.$.derivedFrom], ...element };
			}
		}
		const peripherals: PeripheralOptions[] = [];
		for (const key in peripheralMap) {
			peripherals.push(this.parsePeripheral(peripheralMap[key], defaults));
		}
		// PeripheralNode.compare (nothing is pinned yet)
		peripherals.sort((a, b) => {
			const ga = a.groupName ?? '';
			const gb = b.groupName ?? '';
			if (ga !== gb) {
				return ga > gb ? 1 : -1;
			}
			return a.name === b.name ? 0 : a.name > b.name ? 1 : -1;
		});
		// resolveDeferedEnums: the view resolves them with the names of the whole file.
		for (const { field, derivedFrom } of this.deferred) {
			const map = this.enumTypeValuesMap[derivedFrom];
			if (!map) {
				throw new Error(`Invalid derivedFrom=${derivedFrom} for enumeratedValues of field ${field.name}`);
			}
			field.enumeration = map;
		}
		return peripherals;
	}

	/** `scope`: names of the register and its parents, innermost first. */
	private parseFields(fieldInfo: XmlElement[] | undefined, scope: string[]): FieldOptions[] {
		const fields: FieldOptions[] = [];
		for (const f of fieldInfo ?? []) {
			let offset: number;
			let width: number;
			const description = cleanupDescription(f.description ? f.description[0] : '');
			if (f.bitOffset && f.bitWidth) {
				offset = parseInteger(f.bitOffset[0]) ?? 0;
				width = parseInteger(f.bitWidth[0]) ?? 0;
			} else if (f.bitRange) {
				const range = (f.bitRange[0] as string).substring(1, f.bitRange[0].length - 1).split(':');
				const end = parseInteger(range[0]) ?? 0;
				const start = parseInteger(range[1]) ?? 0;
				width = end - start + 1;
				offset = start;
			} else if (f.msb && f.lsb) {
				const msb = parseInteger(f.msb[0]) ?? 0;
				const lsb = parseInteger(f.lsb[0]) ?? 0;
				width = msb - lsb + 1;
				offset = lsb;
			} else {
				throw new Error(`Unable to parse SVD file: field ${f.name[0]} must have either bitOffset and bitWidth elements, bitRange Element, or msb and lsb elements.`);
			}
			let valueMap: EnumerationMap | undefined;
			let derivedFrom: string | undefined;
			if (f.enumeratedValues) {
				valueMap = {};
				const eValues = f.enumeratedValues[0];
				if (eValues.$?.derivedFrom) {
					const found = this.enumTypeValuesMap[eValues.$.derivedFrom];
					if (!found) {
						derivedFrom = eValues.$.derivedFrom;
					} else {
						valueMap = found;
					}
				} else if (eValues) {
					for (const ev of eValues.enumeratedValue ?? []) {
						if (ev.value && ev.value.length > 0) {
							const value = parseInteger(ev.value[0]) ?? 0;
							valueMap[value] = { name: ev.name[0], description: cleanupDescription(ev.description ? ev.description[0] : ''), value };
						}
					}
					if (eValues.name && eValues.name[0]) {
						let evName: string = eValues.name[0];
						this.enumTypeValuesMap[evName] = valueMap;
						evName = `${f.name[0]}.${evName}`;
						this.enumTypeValuesMap[evName] = valueMap;
						for (const name of scope) {
							evName = `${name}.${evName}`;
							this.enumTypeValuesMap[evName] = valueMap;
						}
					}
				}
			}
			const base: FieldOptions = { name: f.name[0], description, offset, width, enumeration: valueMap };
			if (f.access) {
				base.accessType = accessTypeFromString(f.access[0]);
			}
			if (f.readAction) {
				base.readAction = readActionFromString(f.readAction[0]);
			}
			const instances: FieldOptions[] = [];
			if (f.dim) {
				const { count, increment, index } = dimIndices(f, 'field');
				for (let i = 0; i < count; i++) {
					instances.push({ ...base, name: (f.name[0] as string).replace('%s', index[i]), offset: offset + increment * i });
				}
			} else {
				instances.push(base);
			}
			if (derivedFrom) {
				for (const field of instances) {
					this.deferred.push({ field, derivedFrom });
				}
			}
			fields.push(...instances);
		}
		return fields;
	}

	/** `scope`: names of the parent cluster/peripheral and its parents, innermost first. */
	private parseRegisters(regInfoOrig: XmlElement[], parentName: string, scope: string[]): RegisterOptions[] {
		const regInfo = [...regInfoOrig];
		const localRegisterMap: Record<string, XmlElement> = {};
		for (const r of regInfo) {
			localRegisterMap[r.name[0]] = r;
			this.peripheralRegisterMap[`${parentName}.${r.name[0]}`] = r;
		}
		regInfo.forEach((r, ix) => {
			const derivedFrom = r.$ ? r.$.derivedFrom : '';
			if (derivedFrom) {
				const nm = r.name[0];
				const from = localRegisterMap[derivedFrom] || this.peripheralRegisterMap[derivedFrom];
				if (!from) {
					throw new Error(`SVD error: Invalid 'derivedFrom' "${derivedFrom}" for register "${nm}"`);
				}
				const combined = { ...from, ...r };
				localRegisterMap[nm] = combined;
				this.peripheralRegisterMap[`${parentName}.${nm}`] = combined;
				regInfo[ix] = combined;
			}
		});
		const registers: RegisterOptions[] = [];
		for (const r of regInfo) {
			const base: Partial<RegisterOptions> = {};
			if (r.access) {
				base.accessType = accessTypeFromString(r.access[0]);
			}
			if (r.size) {
				base.size = parseInteger(r.size[0]) ?? 32;
			}
			if (r.resetValue) {
				base.resetValue = parseInteger(r.resetValue[0]) ?? 0;
			}
			if (r.readAction) {
				base.readAction = readActionFromString(r.readAction[0]);
			}
			const fieldsOf = (name: string) => (r.fields && r.fields.length === 1 ? this.parseFields(r.fields[0].field, [name, ...scope]) : []);
			if (r.dim) {
				const { count, increment, index } = dimIndices(r, 'register');
				const offsetBase = parseInteger(r.addressOffset[0]);
				if (offsetBase === undefined || offsetBase < 0) {
					throw new Error(`Unable to parse SVD file: register ${r.name[0]} has invalid addressOffset ${r.addressOffset[0]}`);
				}
				const descBase = cleanupDescription(r.description ? r.description[0] : '');
				for (let i = 0; i < count; i++) {
					const name = (r.name[0] as string).replace('%s', index[i]);
					registers.push({
						...base,
						name,
						description: descBase.replace('%s', index[i]),
						addressOffset: offsetBase + increment * i,
						fields: fieldsOf(name),
					});
				}
			} else {
				if (!r.addressOffset) {
					throw new Error(`Unable to parse SVD file: register ${r.name[0]} has no addressOffset`);
				}
				registers.push({
					...base,
					name: r.name[0],
					description: cleanupDescription(r.description ? r.description[0] : ''),
					addressOffset: parseInteger(r.addressOffset[0])!,
					fields: fieldsOf(r.name[0]),
				});
			}
		}
		return registers;
	}

	private parseClusters(clusterInfo: XmlElement[] | undefined, scope: string[]): ClusterOptions[] {
		const clusters: ClusterOptions[] = [];
		for (const c of clusterInfo ?? []) {
			const base: Partial<ClusterOptions> = {};
			if (c.access) {
				base.accessType = accessTypeFromString(c.access[0]);
			}
			if (c.size) {
				base.size = parseInteger(c.size[0]) ?? 0;
			}
			if (c.resetValue) {
				base.resetValue = parseInteger(c.resetValue[0]) ?? 0;
			}
			const build = (name: string, description: string, addressOffset: number): ClusterOptions => {
				const inner = [name, ...scope];
				const cluster: ClusterOptions = { ...base, name, description, addressOffset };
				if (c.register) {
					cluster.registers = this.parseRegisters(c.register, name, inner);
				}
				if (c.cluster) {
					cluster.clusters = this.parseClusters(c.cluster, inner);
				}
				return cluster;
			};
			if (c.dim) {
				const { count, increment, index } = dimIndices(c, 'cluster');
				const descBase = cleanupDescription(c.description ? c.description[0] : '');
				const offsetBase = parseInteger(c.addressOffset[0]);
				if (offsetBase === undefined || offsetBase < 0) {
					throw new Error(`Unable to parse SVD file: cluster ${c.name[0]} has an no/invalid addressOffset`);
				}
				for (let i = 0; i < count; i++) {
					clusters.push(build((c.name[0] as string).replace('%s', index[i]), descBase.replace('%s', index[i]), offsetBase + increment * i));
				}
			} else {
				clusters.push(build(c.name[0], cleanupDescription(c.description ? c.description[0] : ''), parseInteger(c.addressOffset[0]) ?? 0));
			}
		}
		return clusters;
	}

	private parsePeripheral(p: XmlElement, defaults: { accessType: number; size: number; resetValue: number }): PeripheralOptions {
		let totalLength = 0;
		for (const ab of p.addressBlock ?? []) {
			const offset = parseInteger(ab.offset[0]);
			const size = parseInteger(ab.size[0]);
			if (offset !== undefined && size !== undefined) {
				totalLength = Math.max(totalLength, offset + size);
			}
		}
		const options: PeripheralOptions = {
			name: p.name[0],
			baseAddress: parseInteger(p.baseAddress ? p.baseAddress[0] : '0')!,
			description: cleanupDescription(p.description ? p.description[0] : ''),
			totalLength,
			accessType: defaults.accessType,
			size: defaults.size,
			resetValue: defaults.resetValue,
		};
		if (p.access) {
			options.accessType = accessTypeFromString(p.access[0]);
		}
		if (p.size) {
			options.size = parseInteger(p.size[0]) ?? options.size;
		}
		if (p.resetValue) {
			options.resetValue = parseInteger(p.resetValue[0]) ?? options.resetValue;
		}
		if (p.groupName) {
			options.groupName = p.groupName[0];
		}
		const registers = p.registers?.[0];
		if (registers?.register) {
			options.registers = this.parseRegisters(registers.register, options.name, [options.name]);
		}
		if (registers?.cluster) {
			options.clusters = this.parseClusters(registers.cluster, [options.name]);
		}
		return options;
	}
}

/** The peripherals of an SVD file, as the peripheral viewer would build them from the file itself. */
export function svdToPeripheralOptions(xml: string): PeripheralOptions[] {
	return new SvdConverter().convert(parseXml(xml));
}
