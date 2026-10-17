/*
 * The peripheral definitions of the SVD files given to the XPeripherals view, converted once per
 * file (in a worker thread) and kept as JSON next to the SVD cache.
 */
import * as crypto from 'crypto';
import * as fs from 'fs';
import * as path from 'path';
import { Worker } from 'worker_threads';
import { PeripheralOptions, PeripheralOptionsList } from './svdPeripherals';

/** Changes whenever the conversion changes, so older JSON files are not used. */
const FORMAT_VERSION = 1;

export class SvdPeripheralStore {
	/** Loading or loaded definitions by SVD path, until the view takes them. */
	private readonly loading = new Map<string, Promise<PeripheralOptions[]>>();
	/** SVD files that could not be converted: the view reads these itself. */
	private readonly failed = new Set<string>();

	constructor(
		private readonly cacheDir: string,
		private readonly log: (line: string) => void,
		private readonly workerPath = path.join(__dirname, 'svdPeripheralsWorker.js'),
	) {}

	/** Whether the view can get the peripherals of `svdPath` from here (not after a failed conversion). */
	canProvide(svdPath: string): boolean {
		return !this.failed.has(svdPath);
	}

	/** Starts converting or reading the definitions, so they are ready when the view asks. */
	prepare(svdPath: string): void {
		if (!this.loading.has(svdPath) && this.canProvide(svdPath)) {
			const pending = this.load(svdPath);
			pending.catch(() => undefined);
			this.loading.set(svdPath, pending);
		}
	}

	/** The peripherals for the view; undefined if the file cannot be converted. */
	async peripherals(svdPath: string): Promise<PeripheralOptionsList | undefined> {
		this.prepare(svdPath);
		const pending = this.loading.get(svdPath);
		// The view keeps what it built until VS Code is closed: no need to hold them here as well.
		this.loading.delete(svdPath);
		if (!pending) {
			return undefined;
		}
		try {
			return PeripheralOptionsList.from(await pending) as PeripheralOptionsList;
		} catch (e) {
			this.failed.add(svdPath);
			this.log(`XPeripherals: cannot convert ${svdPath}, the view reads the SVD file itself from the next session: ${e instanceof Error ? e.message : String(e)}`);
			return undefined;
		}
	}

	/** The JSON file of an SVD file: changes with the file, its size and modification time. */
	async jsonPathOf(svdPath: string): Promise<string> {
		const stat = await fs.promises.stat(svdPath);
		const digest = crypto.createHash('sha256')
			.update(`${FORMAT_VERSION}\n${path.resolve(svdPath)}\n${stat.size}\n${stat.mtimeMs}`)
			.digest('hex')
			.slice(0, 16);
		return path.join(this.cacheDir, `${path.basename(svdPath)}.peripherals.${digest}.json`);
	}

	private async load(svdPath: string): Promise<PeripheralOptions[]> {
		const jsonPath = await this.jsonPathOf(svdPath);
		if (!fs.existsSync(jsonPath)) {
			const t0 = Date.now();
			await fs.promises.mkdir(this.cacheDir, { recursive: true });
			const count = await this.convert(svdPath, jsonPath);
			this.log(`XPeripherals: converted ${path.basename(svdPath)} in ${((Date.now() - t0) / 1000).toFixed(1)}s (${count} peripherals)`);
		}
		const peripherals = JSON.parse(await fs.promises.readFile(jsonPath, 'utf8')) as unknown;
		if (!Array.isArray(peripherals)) {
			throw new Error(`${jsonPath}: not a peripheral list`);
		}
		return peripherals as PeripheralOptions[];
	}

	private convert(svdPath: string, jsonPath: string): Promise<number> {
		return new Promise((resolve, reject) => {
			const worker = new Worker(this.workerPath, { workerData: { svdPath, jsonPath } });
			let settled = false;
			worker.once('message', (message: { count?: number; error?: string }) => {
				settled = true;
				if (message.error !== undefined) {
					reject(new Error(message.error));
				} else {
					resolve(message.count ?? 0);
				}
			});
			worker.once('error', (e) => {
				settled = true;
				reject(e);
			});
			worker.once('exit', (code) => {
				if (!settled) {
					reject(new Error(`conversion worker exited with code ${code}`));
				}
			});
		});
	}
}
