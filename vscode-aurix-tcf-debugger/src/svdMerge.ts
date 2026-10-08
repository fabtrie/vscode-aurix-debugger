/**
 * Merges the peripherals of an extra SVD file (e.g. HSM_TC3xx.svd) into a device SVD, as text:
 * the peripheral viewer reads a single SVD file per session, and the device files are too large
 * (40+ MB) to parse and write back just for this.
 */

export type SvdMergeOptions = {
	/** Device peripherals removed before merging (replaced by the extra file's peripherals). */
	replace?: string[];
	/** Peripherals of the extra file that are not merged. */
	exclude?: string[];
};

type PeripheralBlock = { name: string; start: number; end: number };

/** The top-level <peripheral> elements of an SVD document. */
function peripheralBlocks(xml: string): PeripheralBlock[] {
	const blocks: PeripheralBlock[] = [];
	const open = /<peripheral(\s[^>]*)?>/g;
	for (let m = open.exec(xml); m; m = open.exec(xml)) {
		const end = xml.indexOf('</peripheral>', open.lastIndex);
		if (end < 0) {
			throw new Error('SVD: unterminated <peripheral>');
		}
		const name = /^\s*<name>\s*([^<\s]+)\s*<\/name>/.exec(xml.slice(open.lastIndex, open.lastIndex + 512));
		blocks.push({ name: name?.[1] ?? '', start: m.index, end: end + '</peripheral>'.length });
		open.lastIndex = end;
	}
	return blocks;
}

/**
 * Returns `deviceXml` without the peripherals named in `replace` and with the peripherals of
 * `extraXml` (except `exclude`) appended. Interrupt elements of the merged peripherals are dropped:
 * their numbers are the extra core's (e.g. the HSM's NVIC) and would clash with the device's.
 */
export function mergeSvdPeripherals(deviceXml: string, extraXml: string, opts: SvdMergeOptions = {}): string {
	const replace = new Set(opts.replace ?? []);
	const exclude = new Set(opts.exclude ?? []);

	const added = peripheralBlocks(extraXml)
		.filter((b) => !exclude.has(b.name))
		.map((b) => extraXml.slice(b.start, b.end).replace(/[ \t]*<interrupt>[\s\S]*?<\/interrupt>\r?\n?/g, ''));

	const existing = peripheralBlocks(deviceXml);
	const clash = added
		.map((b) => /<name>\s*([^<\s]+)/.exec(b)?.[1] ?? '')
		.filter((n) => existing.some((e) => e.name === n && !replace.has(n)));
	if (clash.length > 0) {
		throw new Error(`SVD merge: peripherals already defined by the device: ${clash.join(', ')}`);
	}

	let out = '';
	let pos = 0;
	for (const b of existing.filter((e) => replace.has(e.name))) {
		out += deviceXml.slice(pos, b.start);
		pos = b.end;
	}
	out += deviceXml.slice(pos);

	const close = out.lastIndexOf('</peripherals>');
	if (close < 0) {
		throw new Error('SVD merge: device file has no </peripherals>');
	}
	const eol = deviceXml.includes('\r\n') ? '\r\n' : '\n';
	const text = added.map((b) => '    ' + b.trim().replace(/\r?\n/g, eol) + eol).join('');
	const at = out.lastIndexOf('\n', close) + 1;
	return out.slice(0, at) + text + out.slice(at);
}
