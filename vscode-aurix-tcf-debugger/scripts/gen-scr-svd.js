// Generates the SCR (XC800 standby controller) SVD file of the TC3xx from
//   scripts/scr-svd/scr_registers.json  (registers, from extract-scr-registers.py)
//   scripts/scr-svd/scr_fields.json     (bit fields, from extract-scr-fields.py)
// and writes
//   scripts/scr-svd/SCR_TC3xx.svd  (readable copy, for review and other tools)
//   bin/svd/SCR_TC3xx.svd.gz       (bundled; merged into the TC3xx device SVDs, see svd-index.json)
//
// Addresses are the SCR debug monitor's virtual SFR addresses (TC3xx UM part 2, Table 558):
//   0x40000 | page << 15 | (page register & 0x7F) << 8 | RMAP << 7 | (SFR & 0x7F)
// aurix-dbg sends reads in 0x40000-0x7FFFF (reserved segment 0 on the TriCore side) to the SCR,
// selecting the page through the module's page register and restoring it afterwards.
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const root = path.resolve(__dirname, '..');
const dir = path.join(__dirname, 'scr-svd');
const outSvd = path.join(dir, 'SCR_TC3xx.svd');
const outGz = path.join(root, 'bin', 'svd', 'SCR_TC3xx.svd.gz');

const errors = [];
function fail(where, msg) {
	errors.push(`${where}: ${msg}`);
}

const hex = (v, w = 2) => '0x' + (v >>> 0).toString(16).toUpperCase().padStart(w, '0');
const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const sentence = (s) => (s && !/[.:!?]$/.test(s) ? `${s}.` : s);

/** Manual access types: r, rh (hardware sets) read-only; w write-only; rw, rwh read-write. */
function fieldAccess(type) {
	if (type === 'w') {
		return 'wo';
	}
	return /^r[hX]?$/.test(type) ? 'ro' : 'rw';
}

const SVD_ACCESS = { ro: 'read-only', rw: 'read-write', wo: 'write-only' };

/** An SVD name from enumeration text without a name of its own. */
function enumName(text, value) {
	const words = String(text).toUpperCase().replace(/[^A-Z0-9]+/g, ' ').trim().split(' ').filter(Boolean);
	let name = '';
	for (const w of words) {
		if (name.length + w.length > 32) {
			break;
		}
		name += (name ? '_' : '') + w;
	}
	if (!name) {
		return `VALUE_${value}`;
	}
	return /^[0-9]/.test(name) ? `V${name}` : name;
}

function enumValue(text, width, where) {
	const m = /^([0-9A-F]+)([BH])$/.exec(text);
	if (!m) {
		fail(where, `enum value ${text}`);
		return undefined;
	}
	const v = parseInt(m[1], m[2] === 'B' ? 2 : 16);
	if (v >= 2 ** width) {
		fail(where, `enum value ${text} does not fit ${width} bits`);
		return undefined;
	}
	return v;
}

function fieldXml(f, regAccess, where, ind) {
	const width = f.msb - f.lsb + 1;
	let access = fieldAccess(f.type);
	if (access === 'wo' && regAccess !== 'wo') {
		// The peripheral viewer does not read a register with a write-only field.
		access = 'rw';
	}
	const desc = [sentence(f.title), f.description, `[${f.type}]`].filter(Boolean).join(' ');
	const lines = [
		`${ind}<field>`,
		`${ind}  <name>${esc(f.name)}</name>`,
		`${ind}  <description>${esc(desc)}</description>`,
		`${ind}  <bitOffset>${f.lsb}</bitOffset>`,
		`${ind}  <bitWidth>${width}</bitWidth>`,
		`${ind}  <access>${SVD_ACCESS[access]}</access>`,
	];
	if (f.enum.length > 0) {
		const names = new Set();
		const values = new Set();
		lines.push(`${ind}  <enumeratedValues>`);
		for (const [text, rawName, d] of f.enum) {
			const v = enumValue(text, width, `${where}.${f.name}`);
			if (v === undefined || values.has(v)) {
				continue;
			}
			values.add(v);
			let name = (rawName || enumName(d, v)).replace(/[^A-Za-z0-9_]/g, '_');
			for (let i = 2; names.has(name); i++) {
				name = `${rawName || enumName(d, v)}_${i}`.replace(/[^A-Za-z0-9_]/g, '_');
			}
			names.add(name);
			lines.push(`${ind}    <enumeratedValue>`);
			lines.push(`${ind}      <name>${esc(name)}</name>`);
			lines.push(`${ind}      <description>${esc(d || name)}</description>`);
			lines.push(`${ind}      <value>${v}</value>`);
			lines.push(`${ind}    </enumeratedValue>`);
		}
		lines.push(`${ind}  </enumeratedValues>`);
	}
	lines.push(`${ind}</field>`);
	return lines;
}

function registerXml(reg, fields, base, ind, alternate) {
	const where = `${reg.module}.${reg.name}`;
	const addr = parseInt(reg.virtualAddress, 16);
	const accesses = new Set(fields.map((f) => fieldAccess(f.type)));
	const access = accesses.size === 1 ? [...accesses][0] : 'rw';
	const where2 = [`SFR ${hex(parseInt(reg.sfr, 16))}`];
	if (reg.pageRegister) {
		where2.push(`${reg.pageRegister} page ${reg.page}`);
	}
	if (reg.rmap !== null) {
		where2.push(`RMAP ${reg.rmap}`);
	}
	const lines = [
		`${ind}<register>`,
		`${ind}  <name>${esc(reg.name)}</name>`,
		`${ind}  <description>${esc(`${reg.description} (${where2.join(', ')})`)}</description>`,
		...(alternate ? [`${ind}  <alternateRegister>${esc(alternate)}</alternateRegister>`] : []),
		`${ind}  <addressOffset>${hex(addr - base, 1)}</addressOffset>`,
		`${ind}  <size>8</size>`,
		`${ind}  <access>${SVD_ACCESS[access]}</access>`,
		`${ind}  <resetValue>${reg.reset ?? '0x00'}</resetValue>`,
		`${ind}  <resetMask>${reg.reset ? reg.resetMask : '0x00'}</resetMask>`,
	];
	if (fields.length > 0) {
		let used = 0;
		const names = new Set();
		lines.push(`${ind}  <fields>`);
		for (const f of [...fields].sort((a, b) => a.lsb - b.lsb)) {
			const mask = ((1 << (f.msb - f.lsb + 1)) - 1) << f.lsb;
			if (f.lsb < 0 || f.msb > 7 || f.lsb > f.msb) {
				fail(where, `${f.name}: bits ${f.msb}:${f.lsb}`);
				continue;
			}
			if (used & mask) {
				fail(where, `${f.name} overlaps another field`);
			}
			if (names.has(f.name)) {
				fail(where, `duplicate field ${f.name}`);
			}
			used |= mask;
			names.add(f.name);
			lines.push(...fieldXml(f, access, where, `${ind}    `));
		}
		lines.push(`${ind}  </fields>`);
	} else {
		fail(where, 'no fields');
	}
	lines.push(`${ind}</register>`);
	return lines;
}

/** Address blocks: one per 128-byte SFR window (one page of one page register) in use. */
function addressBlocks(addrs, base) {
	const windows = new Map();
	for (const a of addrs) {
		const w = a & ~0x7F;
		const [lo, hi] = windows.get(w) ?? [a, a];
		windows.set(w, [Math.min(lo, a), Math.max(hi, a)]);
	}
	return [...windows.values()].sort((x, y) => x[0] - y[0]).map(([lo, hi]) => ({ offset: lo - base, size: hi - lo + 1 }));
}

function peripheralXml(mod, regs, fieldsOf) {
	const ind = '    ';
	const addrs = regs.map((r) => parseInt(r.virtualAddress, 16));
	const base = Math.min(...addrs) & ~0x7F;
	const lines = [
		`${ind}<peripheral>`,
		`${ind}  <name>SCR_${esc(mod.name)}</name>`,
		`${ind}  <description>${esc(`SCR ${mod.description}`)}</description>`,
		`${ind}  <groupName>SCR</groupName>`,
		`${ind}  <baseAddress>${hex(base, 8)}</baseAddress>`,
	];
	for (const b of addressBlocks(addrs, base)) {
		lines.push(`${ind}  <addressBlock>`);
		lines.push(`${ind}    <offset>${hex(b.offset, 1)}</offset>`);
		lines.push(`${ind}    <size>${hex(b.size, 1)}</size>`);
		lines.push(`${ind}    <usage>registers</usage>`);
		lines.push(`${ind}  </addressBlock>`);
	}
	lines.push(`${ind}  <registers>`);
	const names = new Set();
	const seen = new Map();
	for (const r of [...regs].sort((a, b) => parseInt(a.virtualAddress, 16) - parseInt(b.virtualAddress, 16))) {
		if (names.has(r.name)) {
			fail(`${mod.name}.${r.name}`, 'duplicate register name');
		}
		names.add(r.name);
		// Two views of one SFR (SSC_CON in programming and operating mode): the later one is an
		// alternate of the first.
		const alternate = seen.get(r.virtualAddress);
		seen.set(r.virtualAddress, alternate ?? r.name);
		lines.push(...registerXml(r, fieldsOf(r), base, `${ind}    `, alternate));
	}
	lines.push(`${ind}  </registers>`);
	lines.push(`${ind}</peripheral>`);
	return lines;
}

function deviceXml(spec, tables) {
	const lines = [
		'<?xml version="1.0" encoding="utf-8"?>',
		'<!-- Generated by scripts/gen-scr-svd.js from scripts/scr-svd/scr_registers.json and scr_fields.json',
		'     (AURIX TC3xx user manual part 2, chapter 49 SCR). Do not edit. -->',
		'<device schemaVersion="1.3" xmlns:xs="http://www.w3.org/2001/XMLSchema-instance" xs:noNamespaceSchemaLocation="CMSIS-SVD.xsd">',
		'  <vendor>Infineon</vendor>',
		'  <name>SCR_TC3xx</name>',
		'  <version>1.0</version>',
		'  <description>AURIX TC3xx standby controller (SCR, XC800): special function registers at the SCR debug monitor\'s virtual addresses</description>',
		'  <addressUnitBits>8</addressUnitBits>',
		'  <width>8</width>',
		'  <size>8</size>',
		'  <peripherals>',
	];
	let count = 0;
	for (const mod of spec.modules) {
		const regs = spec.registers.filter((r) => r.module === mod.name);
		if (regs.length === 0) {
			continue;
		}
		count += regs.length;
		lines.push(...peripheralXml(mod, regs, (r) => tables[String(r.manualTable)] ?? []));
	}
	const unplaced = spec.registers.filter((r) => !spec.modules.some((m) => m.name === r.module));
	for (const r of unplaced) {
		fail(r.name, `unknown module ${r.module}`);
	}
	lines.push('  </peripherals>', '</device>', '');
	return { xml: lines.join('\n'), count };
}

function main() {
	const spec = JSON.parse(fs.readFileSync(path.join(dir, 'scr_registers.json'), 'utf8'));
	const { tables } = JSON.parse(fs.readFileSync(path.join(dir, 'scr_fields.json'), 'utf8'));
	const { xml, count } = deviceXml(spec, tables);
	if (errors.length > 0) {
		console.error(errors.join('\n'));
		process.exit(1);
	}
	fs.writeFileSync(outSvd, xml);
	fs.writeFileSync(outGz, zlib.gzipSync(Buffer.from(xml), { level: zlib.constants.Z_BEST_COMPRESSION }));
	const fields = (xml.match(/<field>/g) ?? []).length;
	console.log(`${path.relative(root, outSvd)}, ${path.relative(root, outGz)}: ${spec.modules.length} peripherals, ${count} registers, ${fields} fields`);
}

main();
