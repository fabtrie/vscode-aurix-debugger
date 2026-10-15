import * as path from 'path';
import * as fs from 'fs';
import { execFileSync } from 'child_process';
import { downloadAndUnzipVSCode, resolveCliArgsFromVSCodeExecutablePath, runTests } from '@vscode/test-electron';

async function main() {
	const extensionDevelopmentPath = path.resolve(__dirname, '../../');
	const extensionTestsPath = path.resolve(__dirname, './suite/index');

	// Use a stable location for extensions/user-data so we can preinstall extensionDependencies.
	const vscodeTestRoot = path.resolve(extensionDevelopmentPath, '.vscode-test');
	const extensionsDir = path.resolve(vscodeTestRoot, 'extensions');
	const userDataDir = path.resolve(vscodeTestRoot, 'user-data');
	fs.mkdirSync(extensionsDir, { recursive: true });
	fs.mkdirSync(userDataDir, { recursive: true });

	// Our extension relies on other VS Code extensions. In the VS Code test host those are not
	// installed automatically, so we install them before running tests.
	const dependencyIds = ['mcu-debug.peripheral-viewer', 'eclipse-cdt.memory-inspector'];
	const vscodeExecutablePath = await downloadAndUnzipVSCode();
	const [cliPath, ...cliArgs] = resolveCliArgsFromVSCodeExecutablePath(vscodeExecutablePath);

	for (const id of dependencyIds) {
		const alreadyInstalled = fs.readdirSync(extensionsDir, { withFileTypes: true })
			.filter(d => d.isDirectory())
			.some(d => d.name.toLowerCase().startsWith(id.toLowerCase() + '-'));
		if (alreadyInstalled) {
			continue;
		}
		try {
			const installArgs = [
				...cliArgs,
				'--extensions-dir',
				extensionsDir,
				'--user-data-dir',
				userDataDir,
				'--install-extension',
				id,
				'--force',
			];
			// Windows can throw EINVAL when execFileSync runs a .cmd directly.
			if (process.platform === 'win32') {
				execFileSync('cmd.exe', ['/c', cliPath, ...installArgs], { stdio: 'inherit' });
			} else {
				execFileSync(cliPath, installArgs, { stdio: 'inherit' });
			}
		} catch (e) {
			throw new Error(
				`Failed to install required test dependency '${id}'.\n` +
					`Either install it manually into ${extensionsDir} or ensure the VS Code marketplace is reachable.\n` +
					`Original error: ${String(e)}`
			);
		}
	}

	// The stress test expects a workspace folder to be open.
	// Prefer an explicit workspace path if provided, else fall back to the extension root.
	const workspacePath = (process.env.AURIX_TEST_WORKSPACE || '').trim() || extensionDevelopmentPath;
	await runTests({
		extensionDevelopmentPath,
		extensionTestsPath,
		vscodeExecutablePath,
		launchArgs: [workspacePath, '--extensions-dir', extensionsDir, '--user-data-dir', userDataDir],
	});
}

main().catch((err) => {
	console.error(err);
	process.exit(1);
});
