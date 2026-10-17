import * as assert from 'assert';
import * as path from 'path';
import * as vscode from 'vscode';
import { PeripheralOptionsList } from '../../svdPeripherals';

// out/test/suite -> extension root
const extensionPath = path.resolve(__dirname, '../../../');

suite('SVD peripherals command', () => {
	// The duplicate fix of PeripheralOptionsList only works if the view gets the list itself.
	test('a command of the extension host returns its result as is', async () => {
		const list = PeripheralOptionsList.from([{ name: 'X', baseAddress: 0, totalLength: 0, description: '' }]);
		const registration = vscode.commands.registerCommand('aurix-debugger.test.commandResult', () => list);
		try {
			assert.strictEqual(await vscode.commands.executeCommand('aurix-debugger.test.commandResult'), list);
		} finally {
			registration.dispose();
		}
	});

	test('gives the XPeripherals view the peripherals of a session\'s SVD file', async () => {
		await vscode.extensions.getExtension('fabtrie.aurix-debugger')?.activate();
		const svdPath = path.join(extensionPath, 'bin', 'svd', 'HSM_TC3xx.svd.gz');
		const session = { configuration: { svdPath } };
		const peripherals = await vscode.commands.executeCommand<PeripheralOptionsList | undefined>('aurix-debugger.svdPeripherals', session);
		assert.ok(peripherals instanceof PeripheralOptionsList);
		assert.ok(peripherals.some((p) => p.name.startsWith('HSM')), peripherals.map((p) => p.name).join(', '));
		assert.strictEqual(await vscode.commands.executeCommand('aurix-debugger.svdPeripherals', { configuration: {} }), undefined);
	});
});
