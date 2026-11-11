/**
 * `vscode:uninstall` hook: VS Code runs this with Node once the extension is really removed (on
 * the next start, not when it is disabled). It stops the debugger hub, removes its logon start
 * and its installed files, unless another VS Code profile or edition still uses them (the hub
 * keeps a list of the extension directories that installed it).
 */
import * as fs from 'fs';
import * as path from 'path';
import { execFileSync } from 'child_process';

function main(): void {
	if (process.platform !== 'win32') {
		return;
	}
	const extensionPath = path.resolve(__dirname, '..');
	const base = process.env.LOCALAPPDATA ?? path.join(process.env.USERPROFILE ?? '', 'AppData', 'Local');
	const installed = path.join(base, 'aurix-dbg', 'current', 'aurix-hub.exe');
	const bundled = path.join(extensionPath, 'bin', 'aurix-hub.exe');
	const exe = fs.existsSync(installed) ? installed : bundled;
	if (!fs.existsSync(exe)) {
		return;
	}
	try {
		const out = execFileSync(exe, ['uninstall', '--extension', extensionPath], { windowsHide: true, timeout: 60_000 });
		console.log(out.toString());
	} catch (e) {
		console.error(`aurix-hub uninstall failed: ${e instanceof Error ? e.message : String(e)}`);
	}
}

main();
