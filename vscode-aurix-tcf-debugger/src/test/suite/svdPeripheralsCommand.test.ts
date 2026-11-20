import * as assert from 'assert';
import * as vscode from 'vscode';
import { PeripheralOptionsList } from '../../peripheralOptions';

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

	test('has no definitions for a session without a debug server or device', async () => {
		await vscode.extensions.getExtension('fabtrie.aurix-debugger')?.activate();
		// The definitions come from the session's debug server (GET .../svd/definitions); the
		// server side is tested with it (crates/api tests/sim_http.rs).
		assert.strictEqual(await vscode.commands.executeCommand('aurix-debugger.svdPeripherals', { id: 'none', configuration: { device: 'TC37x' } }), undefined);
		assert.strictEqual(await vscode.commands.executeCommand('aurix-debugger.svdPeripherals', { configuration: {} }), undefined);
	});
});
