/*
 * "Add to Live Watch" for registers and fields in the XPeripherals view (mcu-debug.peripheral-viewer).
 *
 * The command is contributed to that extension's view, so VS Code passes its tree node
 * (PeripheralRegisterNode / PeripheralFieldNode). Only their public properties are used.
 */

/** A peripheral register or field as stored in the Live Watch. */
export type LiveWatchPeripheral = {
	address: number;
	/** Register size in bits. */
	sizeBits: number;
	/** Set for a field: its bits within the register. */
	bitOffset?: number;
	bitWidth?: number;
};

export type PeripheralWatchEntry = LiveWatchPeripheral & {
	/** e.g. "STM0.TIM0" or "STM0.TIM0.STM31_0" */
	label: string;
	/** The SVD declares a read action (e.g. clear on read), so every refresh changes the register. */
	hasReadSideEffect: boolean;
};

/** Reads the register or field from an XPeripherals tree node, or undefined for other nodes. */
export function peripheralWatchEntryFromNode(node: any): PeripheralWatchEntry | undefined {
	const isField = typeof node?.width === 'number' && typeof node?.offset === 'number' && node?.parent !== undefined;
	const register = isField ? node.parent : node;
	if (typeof register?.getAddress !== 'function' || typeof register?.size !== 'number') {
		return undefined;
	}
	const address = register.getAddress();
	if (typeof address !== 'number' || !Number.isFinite(address)) {
		return undefined;
	}
	const names: string[] = [];
	for (let n = node; n; n = n.parent) {
		if (typeof n.name === 'string' && n.name !== '') {
			names.unshift(n.name);
		}
	}
	return {
		label: names.join('.'),
		address,
		sizeBits: register.size,
		bitOffset: isField ? node.offset : undefined,
		bitWidth: isField ? node.width : undefined,
		hasReadSideEffect: Boolean(register.readAction) || Boolean(isField && node.readAction),
	};
}

/** C expression reading the whole register, e.g. "*(unsigned int*)0xf0000210". */
export function peripheralExpression(p: LiveWatchPeripheral): string {
	const type = p.sizeBits <= 8 ? 'unsigned char'
		: p.sizeBits <= 16 ? 'unsigned short'
			: p.sizeBits <= 32 ? 'unsigned int'
				: 'unsigned long long';
	return `*(${type}*)0x${p.address.toString(16)}`;
}

export function isLiveWatchPeripheral(value: any): value is LiveWatchPeripheral {
	const isCount = (n: unknown) => typeof n === 'number' && Number.isInteger(n) && n >= 0;
	return isCount(value?.address) && isCount(value?.sizeBits) && value.sizeBits > 0
		&& (value.bitWidth === undefined || (isCount(value.bitWidth) && isCount(value.bitOffset)));
}
