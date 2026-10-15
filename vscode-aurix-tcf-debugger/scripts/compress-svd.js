const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

function collectSvdFiles(dir) {
	if (!fs.existsSync(dir)) {
		return [];
	}
	return fs
		.readdirSync(dir, { withFileTypes: true })
		.filter((entry) => entry.isFile())
		.map((entry) => entry.name)
		.filter((name) => name.toLowerCase().endsWith('.svd'))
		.map((name) => path.join(dir, name));
}

function compressFile(inputPath, outputPath) {
	return new Promise((resolve, reject) => {
		const gzip = zlib.createGzip({ level: zlib.constants.Z_BEST_COMPRESSION });
		const inStream = fs.createReadStream(inputPath);
		const outStream = fs.createWriteStream(outputPath);

		inStream.on('error', reject);
		outStream.on('error', reject);
		outStream.on('finish', resolve);

		inStream.pipe(gzip).pipe(outStream);
	});
}

async function main() {
	const projectRoot = path.resolve(__dirname, '..');
	const svdDir = path.join(projectRoot, 'bin', 'svd');
	const deleteOriginal = process.argv.includes('--delete');

	const files = collectSvdFiles(svdDir);
	if (files.length === 0) {
		console.log('No .svd files found under bin/svd');
		return;
	}

	for (const inputPath of files) {
		const outputPath = `${inputPath}.gz`;
		await compressFile(inputPath, outputPath);
		const inSize = fs.statSync(inputPath).size;
		const outSize = fs.statSync(outputPath).size;
		const ratio = inSize > 0 ? ((1 - outSize / inSize) * 100).toFixed(1) : '0.0';
		console.log(`${path.basename(inputPath)} -> ${path.basename(outputPath)} (${ratio}% smaller)`);
		if (deleteOriginal) {
			fs.unlinkSync(inputPath);
		}
	}

	if (deleteOriginal) {
		console.log('Deleted original .svd files.');
	} else {
		console.log('Original .svd files kept. Re-run with --delete to remove originals.');
	}
}

main().catch((err) => {
	console.error(err instanceof Error ? err.message : String(err));
	process.exit(1);
});
