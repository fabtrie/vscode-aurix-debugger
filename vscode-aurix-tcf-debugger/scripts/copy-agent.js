// Copies aurix-dbg.exe and OpenOCD's Tcl scripts into bin/ so they are packaged
// with the extension.
//
// aurix-dbg.exe, first match wins:
//   1. the environment variable AURIX_DBG_EXE
//   2. ../aurix-debugger/server/target/release/aurix-dbg.exe
//   3. ../aurix-debugger/server/target/debug/aurix-dbg.exe
//
// OpenOCD scripts (tcl/, COPYING, LICENSES/) -> bin/openocd/, first match wins:
//   1. the environment variable AURIX_OPENOCD_DIR (a folder containing tcl/)
//   2. ../aurix-debugger/server/target/openocd-tcl (fetched by the server build,
//      crate openocd-tcl)
//
// The license texts of the third-party code in aurix-dbg.exe -> bin/THIRD_PARTY_LICENSES.txt,
// generated from ../aurix-debugger/server by third-party-licenses.js.
const fs = require('fs');
const path = require('path');
const { generate: generateLicenses } = require('./third-party-licenses');

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

	const openocdSrc = process.env.AURIX_OPENOCD_DIR
		? path.resolve(process.env.AURIX_OPENOCD_DIR)
		: path.join(serverTarget, 'openocd-tcl');
	if (!fs.existsSync(path.join(openocdSrc, 'tcl'))) {
		console.error(`OpenOCD scripts not found in ${openocdSrc}\nBuild the server (cargo build) or set AURIX_OPENOCD_DIR to a folder containing tcl/.`);
		process.exit(1);
	}
	const openocdDest = path.join(projectRoot, 'bin', 'openocd');
	fs.rmSync(openocdDest, { recursive: true, force: true });
	for (const name of ['tcl', 'COPYING', 'LICENSES']) {
		if (fs.existsSync(path.join(openocdSrc, name))) {
			fs.cpSync(path.join(openocdSrc, name), path.join(openocdDest, name), { recursive: true });
		}
	}
	console.log(`Copied ${openocdSrc} -> ${openocdDest}`);

	const licenses = path.join(projectRoot, 'bin', 'THIRD_PARTY_LICENSES.txt');
	try {
		const r = generateLicenses(path.dirname(serverTarget), licenses);
		console.log(`Wrote ${licenses}: ${r.components} components, ${r.texts} license texts`);
	} catch (e) {
		console.error(`Third-party license texts not written: ${e.message || e}`);
		process.exit(1);
	}
}

main();
