import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import { gunzipSync } from 'zlib';
import { findSvdForDevice } from '../../svdCatalog';
import { mergeSvdPeripherals } from '../../svdMerge';

// out/test/suite -> extension root
const extensionPath = path.resolve(__dirname, '../../../');

const device = [
	'<?xml version="1.0" encoding="utf-8"?>',
	'<device schemaVersion="1.3">',
	'  <peripherals>',
	'    <peripheral>',
	'      <name>STM</name>',
	'    </peripheral>',
	'    <peripheral>',
	'      <name>HSM</name>',
	'      <baseAddress>0xF0040000</baseAddress>',
	'    </peripheral>',
	'  </peripherals>',
	'</device>',
	'',
].join('\r\n');

const extra = [
	'<device>',
	'  <peripherals>',
	'    <peripheral>',
	'      <name>HSM_BRIDGE</name>',
	'      <interrupt>',
	'        <name>X</name>',
	'        <value>3</value>',
	'      </interrupt>',
	'      <registers/>',
	'    </peripheral>',
	'    <peripheral>',
	'      <name>HSM_BRIDGE_HSMSIDE</name>',
	'    </peripheral>',
	'  </peripherals>',
	'</device>',
	'',
].join('\n');

function peripheralNames(xml: string): string[] {
	return [...xml.matchAll(/<peripheral(?:\s[^>]*)?>\s*<name>([^<]+)<\/name>/g)].map((m) => m[1]);
}

suite('SVD merge', () => {
	test('replaces, excludes and drops interrupts', () => {
		const out = mergeSvdPeripherals(device, extra, { replace: ['HSM'], exclude: ['HSM_BRIDGE_HSMSIDE'] });
		assert.deepStrictEqual(peripheralNames(out), ['STM', 'HSM_BRIDGE']);
		assert.ok(!out.includes('<interrupt>'));
		assert.ok(out.includes('      <registers/>\r\n    </peripheral>\r\n  </peripherals>\r\n</device>'), out);
		assert.ok(!/[^\r]\n/.test(out), 'keeps the device file line endings');
	});

	test('refuses to duplicate a device peripheral', () => {
		assert.throws(() => mergeSvdPeripherals(device.replace('<name>STM</name>', '<name>HSM_BRIDGE</name>'), extra), /HSM_BRIDGE/);
	});

	test('merges the HSM peripherals into the TC37x SVD', async () => {
		const entry = await findSvdForDevice(extensionPath, 'TC37x');
		assert.ok(entry);
		assert.strictEqual(entry.extraSvd.length, 1);
		const hsm = entry.extraSvd[0];
		const out = mergeSvdPeripherals(
			gunzipSync(fs.readFileSync(entry.absolutePath)).toString('utf8'),
			gunzipSync(fs.readFileSync(hsm.absolutePath)).toString('utf8'),
			hsm
		);
		const names = peripheralNames(out);
		assert.ok(!names.includes('HSM'));
		assert.ok(!names.includes('HSM_BRIDGE_HSMSIDE'));
		for (const n of ['HSM_BRIDGE', 'HSM_AES', 'HSM_HASH', 'HSM_PKC', 'HSM_PKC_SCM', 'HSM_TRNG', 'HSM_SCB', 'HSM_FPB']) {
			assert.ok(names.includes(n), n);
		}
		assert.strictEqual(new Set(names).size, names.length, 'peripheral names are unique');
	});

	test('the TC4x SVD has no HSM_TC3xx peripherals', async () => {
		assert.deepStrictEqual((await findSvdForDevice(extensionPath, 'TC4Dx'))?.extraSvd, []);
	});
});
