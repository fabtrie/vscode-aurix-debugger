// Copies the repository's README.md into the extension folder for packaging.
// vsce only packages files inside the extension folder (--readme-path cannot point outside
// it). The README's relative links are relative to the repository root; the package
// scripts pass that root as --baseContentUrl/--baseImagesUrl so vsce rewrites them.
const fs = require('fs');
const path = require('path');

const projectRoot = path.resolve(__dirname, '..');
const src = path.join(projectRoot, '..', 'README.md');
const dest = path.join(projectRoot, 'README.md');

if (!fs.existsSync(src)) {
	console.error(`copy-readme: ${src} not found`);
	process.exit(1);
}
fs.copyFileSync(src, dest);
console.log(`copy-readme: ${path.relative(projectRoot, src)} -> README.md`);
