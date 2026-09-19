import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import { findSvdForDevice, getSvdCatalogEntries } from '../../svdCatalog';

// out/test/suite -> extension root
const extensionPath = path.resolve(__dirname, '../../../');

suite('SVD catalog', () => {
	test('launch schema "device" enum matches the bundled SVD index', async () => {
		const pkg = JSON.parse(fs.readFileSync(path.join(extensionPath, 'package.json'), 'utf8'));
		const debuggers: any[] = pkg.contributes.debuggers;
		const device = debuggers.find((d) => d.type === 'aurixdbg').configurationAttributes.launch.properties.device;
		const indexed = (await getSvdCatalogEntries(extensionPath)).map((e) => e.device);
		assert.deepStrictEqual([...device.enum].sort(), [...indexed].sort());
		assert.strictEqual(device.enumDescriptions.length, device.enum.length);
	});

	test('finds the SVD by device name or SVD id, case-insensitive', async () => {
		assert.strictEqual((await findSvdForDevice(extensionPath, 'TC37x'))?.id, 'TC37XPD');
		assert.strictEqual((await findSvdForDevice(extensionPath, 'tc37xext'))?.id, 'TC37XEXTPD');
		assert.strictEqual((await findSvdForDevice(extensionPath, 'TC37XPD'))?.device, 'TC37x');
		assert.strictEqual(await findSvdForDevice(extensionPath, 'TC99x'), undefined);
		assert.strictEqual(await findSvdForDevice(extensionPath, ''), undefined);
	});
});
