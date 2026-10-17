/*
 * Worker thread: converts an SVD file to the peripheral definitions JSON (see svdPeripherals.ts),
 * so the conversion of a large file does not block the extension host.
 */
import * as fs from 'fs';
import { parentPort, workerData } from 'worker_threads';
import { gunzipSync } from 'zlib';
import { svdToPeripheralOptions } from './svdPeripherals';

const { svdPath, jsonPath } = workerData as { svdPath: string; jsonPath: string };
try {
	const data = fs.readFileSync(svdPath);
	const xml = (svdPath.toLowerCase().endsWith('.gz') ? gunzipSync(data) : data).toString('utf8');
	const peripherals = svdToPeripheralOptions(xml);
	const tempPath = `${jsonPath}.${process.pid}.tmp`;
	fs.writeFileSync(tempPath, JSON.stringify(peripherals));
	fs.renameSync(tempPath, jsonPath);
	parentPort?.postMessage({ count: peripherals.length });
} catch (e) {
	parentPort?.postMessage({ error: e instanceof Error ? e.message : String(e) });
}
