const fs = require('fs');
const path = require('path');

function ensureDir(dirPath) {
	fs.mkdirSync(dirPath, { recursive: true });
}

function copyDirFlat({ srcDir, destDir, fileFilter }) {
	if (!fs.existsSync(srcDir)) return;
	ensureDir(destDir);
	for (const entry of fs.readdirSync(srcDir, { withFileTypes: true })) {
		if (!entry.isFile()) continue;
		if (fileFilter && !fileFilter(entry.name)) continue;
		const srcPath = path.join(srcDir, entry.name);
		const destPath = path.join(destDir, entry.name);
		fs.copyFileSync(srcPath, destPath);
	}
}

function removeIfExists(targetPath) {
	try {
		if (fs.existsSync(targetPath)) {
			fs.rmSync(targetPath, { force: true, recursive: true });
		}
	} catch {
		// best-effort cleanup only
	}
}

function main() {
	const projectRoot = path.resolve(__dirname, '..');

	// When sources are deleted, tsc does not remove old outputs.
	// Prune removed modules so they don't get packaged into the VSIX.
	const outDir = path.join(projectRoot, 'out');
	for (const base of ['pcap', 'mocksocket', 'debugConfigPanel']) {
		removeIfExists(path.join(outDir, `${base}.js`));
		removeIfExists(path.join(outDir, `${base}.d.ts`));
		removeIfExists(path.join(outDir, `${base}.js.map`));
	}
	removeIfExists(path.join(outDir, 'validators'));

	copyDirFlat({
		srcDir: path.join(projectRoot, 'src', 'tcf', 'validators'),
		destDir: path.join(projectRoot, 'out', 'tcf', 'validators'),
		fileFilter: (name) => name.endsWith('.js')
	});

	copyDirFlat({
		srcDir: path.join(projectRoot, 'src', 'tcf', 'schema'),
		destDir: path.join(projectRoot, 'out', 'tcf', 'schema'),
		fileFilter: (name) => name.endsWith('.json')
	});
}

main();
