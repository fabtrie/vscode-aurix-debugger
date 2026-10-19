// Writes the license texts of the third-party code built into aurix-dbg.exe, so
// they can ship next to it (bin/THIRD_PARTY_LICENSES.txt).
//
// Covered:
//   - every crate aurix-dbg depends on (normal and build dependencies, Windows
//     GNU target), from `cargo metadata` and the license files in each crate's
//     source folder in the cargo registry
//   - Jim Tcl, whose source the jimtcl crate downloads at build time
//   - Capstone, bundled in the capstone-sys crate
//   - the TAS Client API, which crates/tas is a port of
//
// Needs cargo on PATH and the server's dependencies fetched (any cargo build
// does that); Jim Tcl's license is read from the build output, so build the
// server first.
//
// Usage: node scripts/third-party-licenses.js [server dir] [output file]
//   defaults: ../aurix-debugger/server, bin/THIRD_PARTY_LICENSES.txt
const { execFileSync } = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const TARGET = 'x86_64-pc-windows-gnu';
const LICENSE_FILE = /^(licen[cs]e|copying|notice|copyright)/i;

function licenseFiles(dir) {
	return fs.readdirSync(dir)
		.filter((f) => LICENSE_FILE.test(f) && fs.statSync(path.join(dir, f)).isFile())
		.sort()
		.map((f) => path.join(dir, f));
}

/** Third-party packages aurix-dbg is built from, from `cargo metadata`. */
function crates(serverDir) {
	const meta = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--locked', '--filter-platform', TARGET], {
		cwd: serverDir, maxBuffer: 256 * 1024 * 1024, encoding: 'utf8',
	}));
	const byId = new Map(meta.packages.map((p) => [p.id, p]));
	const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
	const root = meta.packages.find((p) => p.name === 'aurix-dbg' && p.source === null);
	if (!root) {
		throw new Error(`no aurix-dbg package in ${serverDir}`);
	}
	const seen = new Set();
	const todo = [root.id];
	while (todo.length) {
		const id = todo.pop();
		if (seen.has(id)) {
			continue;
		}
		seen.add(id);
		for (const d of nodes.get(id).deps) {
			if (d.dep_kinds.some((k) => k.kind !== 'dev')) {
				todo.push(d.pkg);
			}
		}
	}
	return [...seen].map((id) => byId.get(id)).filter((p) => p.source !== null)
		.sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version, undefined, { numeric: true }));
}

/** Jim Tcl's version and LICENSE, from JIMTCL_SRC_DIR or the newest build output of the pinned version. */
function jimLicense(serverDir) {
	const buildRs = fs.readFileSync(path.join(serverDir, 'crates', 'jimtcl', 'build.rs'), 'utf8');
	const version = /const JIM_VERSION: &str = "([^"]+)"/.exec(buildRs)?.[1];
	if (!version) {
		throw new Error('JIM_VERSION not found in crates/jimtcl/build.rs');
	}
	if (process.env.JIMTCL_SRC_DIR) {
		return { version, file: path.join(process.env.JIMTCL_SRC_DIR, 'LICENSE') };
	}
	const found = [];
	for (const profile of ['release', 'debug']) {
		const build = path.join(serverDir, 'target', profile, 'build');
		if (!fs.existsSync(build)) {
			continue;
		}
		for (const d of fs.readdirSync(build).filter((d) => d.startsWith('jimtcl-'))) {
			const out = path.join(build, d, 'out');
			if (!fs.existsSync(out)) {
				continue;
			}
			const f = path.join(out, `jimtcl-${version}`, 'LICENSE');
			if (fs.existsSync(f)) {
				found.push({ f, mtime: fs.statSync(f).mtimeMs });
			}
		}
	}
	found.sort((a, b) => b.mtime - a.mtime);
	if (!found.length) {
		throw new Error(`Jim Tcl ${version}'s LICENSE not found under ${path.join(serverDir, 'target')}; build the server first or set JIMTCL_SRC_DIR`);
	}
	return { version, file: found[0].f };
}

function generate(serverDir, outFile) {
	const sections = [];
	const add = (title, license, files, note) => {
		if (!files.length) {
			throw new Error(`no license files for ${title}`);
		}
		sections.push({ title, license, note, files });
	};

	const jim = jimLicense(serverDir);
	add(`Jim Tcl ${jim.version}`, 'BSD-2-Clause', [jim.file],
		'Tcl interpreter, compiled into aurix-dbg.exe (https://github.com/msteveb/jimtcl).');
	const tasDir = path.join(serverDir, 'crates', 'tas');
	add('TAS Client API', 'Apache-2.0', licenseFiles(tasDir),
		'crates/tas is a Rust port of its read/write client (https://github.com/Infineon/tas_client_api).');

	const pkgs = crates(serverDir);
	for (const p of pkgs) {
		const dir = path.dirname(p.manifest_path);
		const files = licenseFiles(dir);
		if (p.license_file && !files.some((f) => path.resolve(f) === path.resolve(dir, p.license_file))) {
			files.push(path.resolve(dir, p.license_file));
		}
		if (p.name === 'capstone-sys') {
			// The bundled Capstone C library and its LLVM-derived parts.
			files.push(...licenseFiles(path.join(dir, 'capstone')));
		}
		add(`${p.name} ${p.version}`, p.license || 'see license file', files, p.repository || '');
	}

	// Identical texts (mostly the Apache-2.0 license) are printed once.
	const texts = new Map();
	const textId = (file) => {
		const text = fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n').replace(/\s+$/, '') + '\n';
		const hash = crypto.createHash('sha256').update(text).digest('hex');
		if (!texts.has(hash)) {
			texts.set(hash, { id: texts.size + 1, text, users: [] });
		}
		return texts.get(hash);
	};

	const rule = '='.repeat(78);
	const out = [
		'Third-party software in aurix-dbg.exe',
		rule,
		'',
		'aurix-dbg is released under the MIT license of its repository',
		'(https://github.com/fabtrie/vscode-aurix-debugger). aurix-dbg.exe also contains',
		'the third-party software listed below, each under its own license. The',
		'license texts follow the list; identical texts are printed once.',
		'',
		'Generated by scripts/third-party-licenses.js from aurix-debugger/server/Cargo.lock.',
		'',
		rule,
		'',
	];
	for (const s of sections) {
		const ids = s.files.map((f) => {
			const t = textId(f);
			t.users.push(`${s.title}: ${path.basename(f)}`);
			return `[${t.id}] ${path.basename(f)}`;
		});
		out.push(`${s.title}  (${s.license})`);
		if (s.note) {
			out.push(`  ${s.note}`);
		}
		out.push(`  License texts: ${ids.join(', ')}`, '');
	}
	for (const t of texts.values()) {
		out.push(rule, `[${t.id}] used by:`, ...t.users.map((u) => `  ${u}`), rule, '', t.text);
	}
	fs.mkdirSync(path.dirname(outFile), { recursive: true });
	fs.writeFileSync(outFile, out.join('\n'));
	return { components: sections.length, texts: texts.size };
}

module.exports = { generate };

if (require.main === module) {
	const projectRoot = path.resolve(__dirname, '..');
	const serverDir = path.resolve(process.argv[2] || path.join(projectRoot, '..', 'aurix-debugger', 'server'));
	const outFile = path.resolve(process.argv[3] || path.join(projectRoot, 'bin', 'THIRD_PARTY_LICENSES.txt'));
	try {
		const r = generate(serverDir, outFile);
		console.log(`Wrote ${outFile}: ${r.components} components, ${r.texts} license texts`);
	} catch (e) {
		console.error(String(e.message || e));
		process.exit(1);
	}
}
