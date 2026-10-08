import * as fs from 'fs';
import * as path from 'path';

/** An SVD file whose peripherals are merged into a device SVD (e.g. the HSM of the TC3xx). */
export type SvdExtra = {
	absolutePath: string;
	/** Device peripherals it replaces. */
	replace: string[];
	/** Its peripherals that are not merged. */
	exclude: string[];
};

export type SvdCatalogEntry = {
	id: string;
	/** Device name as used in the launch configuration's "device" (e.g. "TC37x"). */
	device: string;
	label: string;
	relativePath: string;
	absolutePath: string;
	deviceNames: string[];
	extraSvd: SvdExtra[];
};

type RawSvdIndexEntry = {
	id?: unknown;
	device?: unknown;
	label?: unknown;
	path?: unknown;
	deviceNames?: unknown;
	extraSvd?: unknown;
};

type RawSvdIndex = {
	entries?: unknown;
};

const SVD_ROOT_RELATIVE = path.join('bin', 'svd');
const SVD_INDEX_NAME = 'svd-index.json';

function normalizeSvdBaseName(fileName: string): string {
	const lower = fileName.toLowerCase();
	if (lower.endsWith('.svd.gz')) {
		return fileName.slice(0, -7);
	}
	if (lower.endsWith('.svd')) {
		return fileName.slice(0, -4);
	}
	return fileName;
}

function stringList(v: unknown): string[] {
	return Array.isArray(v) ? v.filter((s): s is string => typeof s === 'string' && s.trim() !== '').map((s) => s.trim()) : [];
}

function parseExtra(raw: unknown, extensionPath: string): SvdExtra | undefined {
	const r = (typeof raw === 'string' ? { path: raw } : raw) as { path?: unknown; replace?: unknown; exclude?: unknown } | null;
	if (!r || typeof r.path !== 'string' || r.path.trim() === '') {
		return undefined;
	}
	const absolutePath = path.resolve(extensionPath, r.path.trim().split(/[\\/]+/).join('/'));
	if (!fs.existsSync(absolutePath)) {
		return undefined;
	}
	return { absolutePath, replace: stringList(r.replace), exclude: stringList(r.exclude) };
}

function parseIndexEntry(raw: RawSvdIndexEntry, extensionPath: string): SvdCatalogEntry | undefined {
	if (typeof raw.path !== 'string' || raw.path.trim() === '') {
		return undefined;
	}

	const rel = raw.path.trim().split(/[\\/]+/).join('/');
	const absolutePath = path.resolve(extensionPath, rel);
	if (!fs.existsSync(absolutePath)) {
		return undefined;
	}

	const fallbackId = normalizeSvdBaseName(path.basename(rel));
	const id = typeof raw.id === 'string' && raw.id.trim() ? raw.id.trim() : fallbackId;
	const device = typeof raw.device === 'string' && raw.device.trim() ? raw.device.trim() : id;
	const label = typeof raw.label === 'string' && raw.label.trim() ? raw.label.trim() : id;

	const deviceNames = Array.isArray(raw.deviceNames)
		? raw.deviceNames
			.map((d) => typeof d === 'string' ? d.trim() : '')
			.filter((d) => d.length > 0)
		: [];

	return {
		id,
		device,
		label,
		relativePath: rel,
		absolutePath,
		deviceNames,
		extraSvd: Array.isArray(raw.extraSvd)
			? raw.extraSvd.map((x) => parseExtra(x, extensionPath)).filter((x): x is SvdExtra => x !== undefined)
			: [],
	};
}

/** The SVD files listed in bin/svd/svd-index.json that exist in the extension. */
export async function getSvdCatalogEntries(extensionPath: string): Promise<SvdCatalogEntry[]> {
	const indexPath = path.join(extensionPath, SVD_ROOT_RELATIVE, SVD_INDEX_NAME);
	if (!fs.existsSync(indexPath)) {
		return [];
	}

	let parsed: RawSvdIndex;
	try {
		const raw = await fs.promises.readFile(indexPath, 'utf8');
		parsed = JSON.parse(raw) as RawSvdIndex;
	} catch {
		return [];
	}

	if (!Array.isArray(parsed.entries)) {
		return [];
	}

	const entries: SvdCatalogEntry[] = [];
	for (const item of parsed.entries) {
		if (!item || typeof item !== 'object') {
			continue;
		}
		const parsedEntry = parseIndexEntry(item as RawSvdIndexEntry, extensionPath);
		if (parsedEntry) {
			entries.push(parsedEntry);
		}
	}

	return entries;
}

/** The bundled SVD for a device name ("TC37x"), case-insensitive; the SVD id ("TC37XPD") also matches. */
export async function findSvdForDevice(extensionPath: string, device: string): Promise<SvdCatalogEntry | undefined> {
	const wanted = device.trim().toLowerCase();
	if (wanted === '') {
		return undefined;
	}
	const entries = await getSvdCatalogEntries(extensionPath);
	return entries.find((e) => e.device.toLowerCase() === wanted)
		?? entries.find((e) => [e.id, ...e.deviceNames].some((n) => n.toLowerCase() === wanted));
}
