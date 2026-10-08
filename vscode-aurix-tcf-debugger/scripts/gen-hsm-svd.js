// Generates the HSM SVD file from scripts/hsm-svd/hsm_tc3xx.json:
//   scripts/hsm-svd/HSM_TC3xx.svd  (readable copy, for review and other tools)
//   bin/svd/HSM_TC3xx.svd.gz       (bundled; merged into the TC3xx device SVDs, see svd-index.json)
// Arrays (dim) are expanded here, so the output only uses plain registers and clusters.
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const root = path.resolve(__dirname, '..');
const input = path.join(__dirname, 'hsm-svd', 'hsm_tc3xx.json');
const outSvd = path.join(__dirname, 'hsm-svd', 'HSM_TC3xx.svd');
const outGz = path.join(root, 'bin', 'svd', 'HSM_TC3xx.svd.gz');

const ACCESS = { ro: 'read-only', rw: 'read-write', wo: 'write-only' };

const errors = [];
function fail(where, msg) {
	errors.push(`${where}: ${msg}`);
}

function num(v) {
	return typeof v === 'number' ? v : Number(v);
}

function hex(v) {
	return '0x' + (v >>> 0).toString(16).toUpperCase().padStart(8, '0');
}

function esc(s) {
	return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

/** Expands an item with dim/inc/idx into copies with '%s' replaced and the offset advanced. */
function expand(item) {
	if (!item.dim) {
		return [item];
	}
	const idx = item.idx ?? [...Array(item.dim).keys()];
	if (idx.length !== item.dim || !item.n.includes('%s')) {
		fail(item.n, 'dim needs a name with %s and dim index values');
	}
	return idx.map((k, i) => ({
		...item,
		dim: undefined,
		n: item.n.replace('%s', String(k)),
		d: item.d?.replace('%s', String(k)),
		o: num(item.o) + i * item.inc,
	}));
}

function parseBits(b) {
	const [msb, lsb = msb] = String(b).split(':').map(Number);
	return { msb, lsb };
}

function fieldsOf(reg, periph) {
	if (reg.irqFields) {
		return periph.irqBits.map(([bit, name, d]) => ({ n: name, b: String(bit), d: `${d} interrupt (IRQ ${bit})` }));
	}
	return reg.f ?? [];
}

function registerXml(reg, periph, where, ind) {
	const lines = [];
	const access = reg.a ?? 'rw';
	if (!ACCESS[access]) {
		fail(where, `unknown access ${access}`);
	}
	const reset = reg.r === undefined ? 0 : num(reg.r);
	const resetMask = reg.rm !== undefined ? num(reg.rm) : reg.r === undefined ? 0 : 0xFFFFFFFF;
	lines.push(`${ind}<register>`);
	lines.push(`${ind}  <name>${esc(reg.n)}</name>`);
	lines.push(`${ind}  <description>${esc(reg.d ?? reg.n)}</description>`);
	lines.push(`${ind}  <addressOffset>0x${num(reg.o).toString(16).toUpperCase()}</addressOffset>`);
	lines.push(`${ind}  <size>32</size>`);
	lines.push(`${ind}  <access>${ACCESS[access]}</access>`);
	lines.push(`${ind}  <resetValue>${hex(reset)}</resetValue>`);
	lines.push(`${ind}  <resetMask>${hex(resetMask)}</resetMask>`);
	if (reg.mwv) {
		lines.push(`${ind}  <modifiedWriteValues>${reg.mwv}</modifiedWriteValues>`);
	}
	if (reg.readAction) {
		lines.push(`${ind}  <readAction>${reg.readAction}</readAction>`);
	}
	const fields = fieldsOf(reg, periph);
	if (fields.length > 0) {
		let used = 0n;
		const names = new Set();
		lines.push(`${ind}  <fields>`);
		for (const f of fields) {
			const fw = `${where}.${f.n}`;
			const { msb, lsb } = parseBits(f.b);
			if (!(lsb >= 0 && msb <= 31 && lsb <= msb)) {
				fail(fw, `bad bit range ${f.b}`);
				continue;
			}
			const mask = ((1n << BigInt(msb - lsb + 1)) - 1n) << BigInt(lsb);
			if (used & mask) {
				fail(fw, 'overlaps another field');
			}
			used |= mask;
			if (names.has(f.n)) {
				fail(fw, 'duplicate field name');
			}
			names.add(f.n);
			const fa = f.a ?? (access === 'rw' ? undefined : access);
			if (fa === 'wo' && access !== 'wo') {
				// The peripheral viewer skips reading a register with any write-only field.
				fail(fw, 'write-only field in a readable register');
			}
			lines.push(`${ind}    <field>`);
			lines.push(`${ind}      <name>${esc(f.n)}</name>`);
			lines.push(`${ind}      <description>${esc(f.d ?? f.n)}</description>`);
			lines.push(`${ind}      <lsb>${lsb}</lsb>`);
			lines.push(`${ind}      <msb>${msb}</msb>`);
			lines.push(`${ind}      <access>${ACCESS[fa ?? access]}</access>`);
			if (f.e) {
				const width = msb - lsb + 1;
				lines.push(`${ind}      <enumeratedValues>`);
				for (const [value, name, d] of f.e) {
					if (value >= 2 ** width) {
						fail(fw, `enum ${name} does not fit`);
					}
					lines.push(`${ind}        <enumeratedValue>`);
					lines.push(`${ind}          <name>${esc(name)}</name>`);
					lines.push(`${ind}          <description>${esc(d)}</description>`);
					lines.push(`${ind}          <value>${value}</value>`);
					lines.push(`${ind}        </enumeratedValue>`);
				}
				lines.push(`${ind}      </enumeratedValues>`);
			}
			lines.push(`${ind}    </field>`);
		}
		lines.push(`${ind}  </fields>`);
	}
	lines.push(`${ind}</register>`);
	return lines;
}

/** Checks that the registers (offsets relative to the peripheral) do not overlap and fit the block. */
function checkLayout(periph, spans) {
	spans.sort((a, b) => a.start - b.start);
	for (let i = 1; i < spans.length; i++) {
		if (spans[i].start < spans[i - 1].start + 4) {
			fail(periph.name, `${spans[i].name} overlaps ${spans[i - 1].name}`);
		}
	}
	const size = num(periph.size);
	for (const s of spans) {
		if (s.start % 4 !== 0) {
			fail(periph.name, `${s.name} is not word aligned`);
		}
		if (s.start + 4 > size) {
			fail(periph.name, `${s.name} lies outside the address block`);
		}
	}
}

function peripheralXml(periph) {
	const ind = '    ';
	const lines = [];
	const spans = [];
	lines.push(`${ind}<peripheral>`);
	lines.push(`${ind}  <name>${esc(periph.name)}</name>`);
	lines.push(`${ind}  <description>${esc(periph.description)}</description>`);
	lines.push(`${ind}  <groupName>HSM</groupName>`);
	lines.push(`${ind}  <baseAddress>${hex(num(periph.base))}</baseAddress>`);
	lines.push(`${ind}  <addressBlock>`);
	lines.push(`${ind}    <offset>0x0</offset>`);
	lines.push(`${ind}    <size>0x${num(periph.size).toString(16).toUpperCase()}</size>`);
	lines.push(`${ind}    <usage>registers</usage>`);
	lines.push(`${ind}  </addressBlock>`);
	for (const [value, name, d] of periph.interrupts ?? []) {
		lines.push(`${ind}  <interrupt>`);
		lines.push(`${ind}    <name>${esc(name)}</name>`);
		lines.push(`${ind}    <description>${esc(d)}</description>`);
		lines.push(`${ind}    <value>${value}</value>`);
		lines.push(`${ind}  </interrupt>`);
	}
	lines.push(`${ind}  <registers>`);
	const names = new Set();
	const unique = (name) => {
		if (names.has(name)) {
			fail(periph.name, `duplicate name ${name}`);
		}
		names.add(name);
	};
	for (const reg of (periph.registers ?? []).flatMap(expand)) {
		unique(reg.n);
		spans.push({ name: reg.n, start: num(reg.o) });
		lines.push(...registerXml(reg, periph, `${periph.name}.${reg.n}`, `${ind}    `));
	}
	for (const cl of (periph.clusters ?? []).flatMap(expand)) {
		unique(cl.n);
		lines.push(`${ind}    <cluster>`);
		lines.push(`${ind}      <name>${esc(cl.n)}</name>`);
		lines.push(`${ind}      <description>${esc(cl.d ?? cl.n)}</description>`);
		lines.push(`${ind}      <addressOffset>0x${num(cl.o).toString(16).toUpperCase()}</addressOffset>`);
		for (const reg of cl.registers.flatMap(expand)) {
			spans.push({ name: `${cl.n}.${reg.n}`, start: num(cl.o) + num(reg.o) });
			lines.push(...registerXml(reg, periph, `${periph.name}.${cl.n}.${reg.n}`, `${ind}      `));
		}
		lines.push(`${ind}    </cluster>`);
	}
	lines.push(`${ind}  </registers>`);
	lines.push(`${ind}</peripheral>`);
	checkLayout(periph, spans);
	return lines;
}

function deviceXml(spec) {
	const d = spec.device;
	const c = d.cpu;
	const lines = [
		'<?xml version="1.0" encoding="utf-8"?>',
		'<!-- Generated by scripts/gen-hsm-svd.js from scripts/hsm-svd/hsm_tc3xx.json. Do not edit. -->',
		'<device schemaVersion="1.3" xmlns:xs="http://www.w3.org/2001/XMLSchema-instance" xs:noNamespaceSchemaLocation="CMSIS-SVD.xsd">',
		`  <vendor>${esc(d.vendor)}</vendor>`,
		`  <name>${esc(d.name)}</name>`,
		`  <series>${esc(d.series)}</series>`,
		`  <version>${esc(d.version)}</version>`,
		`  <description>${esc(d.description)}</description>`,
		'  <cpu>',
		`    <name>${c.name}</name>`,
		`    <revision>${c.revision}</revision>`,
		`    <endian>${c.endian}</endian>`,
		`    <mpuPresent>${c.mpuPresent}</mpuPresent>`,
		`    <fpuPresent>${c.fpuPresent}</fpuPresent>`,
		`    <nvicPrioBits>${c.nvicPrioBits}</nvicPrioBits>`,
		`    <vendorSystickConfig>${c.vendorSystickConfig}</vendorSystickConfig>`,
		'  </cpu>',
		'  <addressUnitBits>8</addressUnitBits>',
		'  <width>32</width>',
		'  <size>32</size>',
		'  <peripherals>',
	];
	const names = new Set();
	for (const p of spec.peripherals) {
		if (names.has(p.name)) {
			fail(p.name, 'duplicate peripheral');
		}
		names.add(p.name);
		lines.push(...peripheralXml(p));
	}
	lines.push('  </peripherals>', '</device>', '');
	return lines.join('\n');
}

function main() {
	const spec = JSON.parse(fs.readFileSync(input, 'utf8'));
	const xml = deviceXml(spec);
	if (errors.length > 0) {
		console.error(errors.join('\n'));
		process.exit(1);
	}
	fs.writeFileSync(outSvd, xml);
	fs.writeFileSync(outGz, zlib.gzipSync(Buffer.from(xml), { level: zlib.constants.Z_BEST_COMPRESSION }));
	const regs = (xml.match(/<register>/g) ?? []).length;
	console.log(`${path.relative(root, outSvd)}, ${path.relative(root, outGz)}: ${spec.peripherals.length} peripherals, ${regs} registers`);
}

main();
