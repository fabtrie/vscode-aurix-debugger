// Copies aurix-dbg.exe into bin/ so it is packaged with the extension.
//
// Source, first match wins:
//   1. the environment variable AURIX_DBG_EXE
//   2. ../aurix-debugger/server/target/release/aurix-dbg.exe
//   3. ../aurix-debugger/server/target/debug/aurix-dbg.exe
const fs = require('fs');
const path = require('path');

function main() {
	const projectRoot = path.resolve(__dirname, '..');
	const serverTarget = path.resolve(projectRoot, '..', 'aurix-debugger', 'server', 'target');
	const candidates = process.env.AURIX_DBG_EXE
		? [path.resolve(process.env.AURIX_DBG_EXE)]
		: [path.join(serverTarget, 'release', 'aurix-dbg.exe'), path.join(serverTarget, 'debug', 'aurix-dbg.exe')];
	const src = candidates.find((p) => fs.existsSync(p));
	if (!src) {
		console.error(`aurix-dbg.exe not found. Looked in:\n  ${candidates.join('\n  ')}\nSet AURIX_DBG_EXE to its path.`);
		process.exit(1);
	}

	const dest = path.join(projectRoot, 'bin', 'aurix-dbg.exe');
	fs.mkdirSync(path.dirname(dest), { recursive: true });
	fs.copyFileSync(src, dest);
	console.log(`Copied ${src} -> ${dest}`);
}

main();
