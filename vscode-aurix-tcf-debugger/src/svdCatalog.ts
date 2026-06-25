import * as fs from 'fs';
import * as path from 'path';

export type SvdCatalogEntry = {
	id: string;
	label: string;
	relativePath: string;
	absolutePath: string;
	deviceNames: string[];
};

type RawSvdIndexEntry = {
	id?: unknown;
	label?: unknown;
	path?: unknown;
	deviceNames?: unknown;
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

function toWorkspaceRelativePath(absolutePath: string, extensionPath: string): string {
	const rel = path.relative(extensionPath, absolutePath);
	return rel.split(path.sep).join('/');
}

function isSvdFileName(fileName: string): boolean {
	const lower = fileName.toLowerCase();
	return lower.endsWith('.svd') || lower.endsWith('.svd.gz');
}

async function collectSvdFiles(dir: string): Promise<string[]> {
	const result: string[] = [];
	if (!fs.existsSync(dir)) {
		return result;
	}

	const stack = [dir];
	while (stack.length > 0) {
		const current = stack.pop();
		if (!current) {
			continue;
		}
		const entries = await fs.promises.readdir(current, { withFileTypes: true });
		for (const entry of entries) {
			const fullPath = path.join(current, entry.name);
			if (entry.isDirectory()) {
				stack.push(fullPath);
				continue;
			}
			if (entry.isFile() && isSvdFileName(entry.name)) {
				result.push(fullPath);
			}
		}
	}

	return result;
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
	const label = typeof raw.label === 'string' && raw.label.trim() ? raw.label.trim() : id;

	const deviceNames = Array.isArray(raw.deviceNames)
		? raw.deviceNames
			.map((d) => typeof d === 'string' ? d.trim() : '')
			.filter((d) => d.length > 0)
		: [];

	return {
		id,
		label,
		relativePath: rel,
		absolutePath,
		deviceNames,
	};
}

async function loadIndexedEntries(extensionPath: string): Promise<SvdCatalogEntry[]> {
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

export async function getSvdCatalogEntries(extensionPath: string): Promise<SvdCatalogEntry[]> {
	const byPath = new Map<string, SvdCatalogEntry>();

	for (const entry of await loadIndexedEntries(extensionPath)) {
		byPath.set(path.normalize(entry.absolutePath).toLowerCase(), entry);
	}

	const svdRoot = path.join(extensionPath, SVD_ROOT_RELATIVE);
	for (const absolutePath of await collectSvdFiles(svdRoot)) {
		const key = path.normalize(absolutePath).toLowerCase();
		if (byPath.has(key)) {
			continue;
		}
		const baseName = normalizeSvdBaseName(path.basename(absolutePath));
		byPath.set(key, {
			id: baseName,
			label: baseName,
			relativePath: toWorkspaceRelativePath(absolutePath, extensionPath),
			absolutePath,
			deviceNames: [baseName],
		});
	}

	return Array.from(byPath.values()).sort((a, b) => a.label.localeCompare(b.label));
}

export async function getSvdOptionsForDevice(extensionPath: string, targetDevice?: string): Promise<Array<{ label: string; value: string }>> {
	const entries = await getSvdCatalogEntries(extensionPath);
	void targetDevice;
	return entries.map((entry) => ({ label: entry.label, value: entry.absolutePath }));
}
