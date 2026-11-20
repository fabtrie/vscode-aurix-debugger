/*
 * The peripheral definitions the XPeripherals view (mcu-debug.peripheral-viewer) takes from a
 * command instead of reading an SVD file (launch configuration "peripherals": "command:..."):
 * the options its PeripheralNode is built from. The debug server converts the bundled SVD files
 * into them (GET /api/v1/svd/definitions); this module types them and works around the view's
 * double adding of children.
 */

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
