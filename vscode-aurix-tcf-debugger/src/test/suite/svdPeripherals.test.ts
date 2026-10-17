import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { parseXml, PeripheralOptions, PeripheralOptionsList, svdToPeripheralOptions } from '../../svdPeripherals';
import { SvdPeripheralStore } from '../../svdPeripheralStore';

const svd = `<?xml version="1.0" encoding="utf-8"?>
<!-- comment -->
<device schemaVersion="1.1">
  <name>TEST</name>
  <size>32</size>
  <access>read-write</access>
  <peripherals>
    <peripheral>
      <name>STM0</name>
      <groupName>STM</groupName>
      <description>System
        timer</description>
      <baseAddress>0xF0001000</baseAddress>
      <addressBlock><offset>0x0</offset><size>0x100</size></addressBlock>
      <registers>
        <register>
          <name>CMP%s</name>
          <description>Compare %s</description>
          <dim>2</dim>
          <dimIncrement>4</dimIncrement>
          <addressOffset>0x30</addressOffset>
          <access>read-only</access>
          <fields>
            <field>
              <name>MODE</name>
              <bitRange>[3:0]</bitRange>
              <enumeratedValues derivedFrom="MODES"/>
            </field>
          </fields>
        </register>
        <register>
          <name>TIM0</name>
          <description>a &lt; b &amp; &#x41;</description>
          <addressOffset>0x10</addressOffset>
          <fields>
            <field>
              <name>STAT</name>
              <bitOffset>4</bitOffset>
              <bitWidth>2</bitWidth>
              <access>write-only</access>
              <enumeratedValues>
                <name>MODES</name>
                <enumeratedValue><name>OFF</name><description>Off</description><value>0</value></enumeratedValue>
                <enumeratedValue><name>ON</name><value>0b01</value></enumeratedValue>
              </enumeratedValues>
            </field>
          </fields>
        </register>
        <cluster>
          <name>CH[%s]</name>
          <dim>2</dim>
          <dimIncrement>0x20</dimIncrement>
          <dimIndex>A,B</dimIndex>
          <addressOffset>0x80</addressOffset>
          <register><name>CTRL</name><addressOffset>0x4</addressOffset><resetValue>0x5</resetValue></register>
        </cluster>
      </registers>
    </peripheral>
    <peripheral derivedFrom="STM0">
      <name>STM1</name>
      <baseAddress>0xF0001100</baseAddress>
    </peripheral>
    <peripheral>
      <name>ADC</name>
      <baseAddress>0xF0020000</baseAddress>
      <registers/>
    </peripheral>
  </peripherals>
</device>
`;

/**
 * Node classes that build children like peripheral viewer 1.6.5: a child adds itself to its
 * parent and the parent adds it again; every add sorts the children.
 */
class FakeRegister {
	readonly offset: number;
	constructor(parent: FakePeripheral, options: { addressOffset: number }) {
		this.offset = options.addressOffset;
		parent.addChild(this);
	}
}
class FakePeripheral {
	children: FakeRegister[] = [];
	constructor(options: PeripheralOptions) {
		for (const register of options.registers ?? []) {
			this.addChild(new FakeRegister(this, register));
		}
	}
	addChild(child: FakeRegister): void {
		this.children.push(child);
		this.children.sort((c1, c2) => (c1.offset > c2.offset ? 1 : -1));
	}
}

suite('SVD peripherals', () => {
	test('reads XML like xml2js', () => {
		const doc = parseXml('<?xml version="1.0"?><a x="1 &gt; 0"><b>t</b><b/><c y=\'2\'>u</c><d> </d><![CDATA[ignored]]></a>');
		assert.deepStrictEqual(doc, { a: { $: { x: '1 > 0' }, b: ['t', ''], c: [{ $: { y: '2' }, _: 'u' }], d: [' '], _: 'ignored' } });
		assert.throws(() => parseXml('<a><b></a>'), /unexpected <\/a>/);
		assert.throws(() => parseXml('<a>'), /not closed/);
	});

	test('converts peripherals, registers, fields and clusters like the view', () => {
		const ps = svdToPeripheralOptions(svd);
		// Sorted by group, then name.
		assert.deepStrictEqual(ps.map((p) => p.name), ['ADC', 'STM0', 'STM1']);
		const stm0 = ps[1];
		assert.strictEqual(stm0.description, 'System timer');
		assert.strictEqual(stm0.baseAddress, 0xF0001000);
		assert.strictEqual(stm0.totalLength, 0x100);
		assert.strictEqual(stm0.accessType, 2);
		assert.deepStrictEqual(stm0.registers?.map((r) => [r.name, r.addressOffset, r.description]), [
			['CMP0', 0x30, 'Compare 0'],
			['CMP1', 0x34, 'Compare 1'],
			['TIM0', 0x10, 'a < b & A'],
		]);
		const tim0 = stm0.registers![2];
		assert.deepStrictEqual(tim0.fields, [{
			name: 'STAT', description: '', offset: 4, width: 2, accessType: 3,
			enumeration: { 0: { name: 'OFF', description: 'Off', value: 0 }, 1: { name: 'ON', description: '', value: 1 } },
		}]);
		// An enumeration named later in the file, as the view's resolveDeferedEnums does it.
		const mode = stm0.registers![0].fields![0];
		assert.deepStrictEqual([mode.offset, mode.width, mode.enumeration?.[1]?.name], [0, 4, 'ON']);
		assert.strictEqual(stm0.registers![0].accessType, 1);
		assert.deepStrictEqual(stm0.clusters?.map((c) => [c.name, c.addressOffset, c.registers?.[0].resetValue]), [['CH[A]', 0x80, 5], ['CH[B]', 0xA0, 5]]);
		// derivedFrom: the base peripheral's registers.
		assert.strictEqual(ps[2].baseAddress, 0xF0001100);
		assert.strictEqual(ps[2].registers?.length, 3);
		assert.strictEqual(ps[0].registers, undefined);
	});

	test('fails on an enumeration that is never named', () => {
		assert.throws(() => svdToPeripheralOptions(svd.replace('<name>MODES</name>', '<name>OTHER</name>')), /Invalid derivedFrom=MODES/);
	});

	test('the view builds each child once, sorted', () => {
		const list = PeripheralOptionsList.from(svdToPeripheralOptions(svd)) as PeripheralOptionsList;
		const nodes = list.map((options) => new FakePeripheral(options));
		assert.deepStrictEqual(nodes[1].children.map((r) => r.offset), [0x10, 0x30, 0x34]);
		// addChild is the view's own again afterwards.
		const plain = new FakePeripheral({ name: 'X', baseAddress: 0, totalLength: 0, description: '', registers: [{ name: 'R', addressOffset: 0 }] });
		assert.strictEqual(plain.children.length, 2);
		// Without the view's classes the duplicates are removed afterwards.
		const objects = list.map((options) => {
			const p = { children: [] as object[] };
			for (const r of options.registers ?? []) {
				p.children.push(r, r);
			}
			return p;
		});
		assert.strictEqual(objects[1].children.length, 3);
		// Other array methods return plain arrays.
		assert.strictEqual(Object.getPrototypeOf(list.filter(() => true)), Array.prototype);
	});

	test('the store converts in a worker once and reads the JSON afterwards', async () => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aurix-svd-'));
		try {
			const svdPath = path.join(dir, 'TEST.svd');
			fs.writeFileSync(svdPath, svd);
			const logs: string[] = [];
			const store = new SvdPeripheralStore(path.join(dir, 'cache'), (line) => logs.push(line), path.resolve(__dirname, '../../svdPeripheralsWorker.js'));
			store.prepare(svdPath);
			const first = await store.peripherals(svdPath);
			assert.deepStrictEqual(first?.map((p) => p.name), ['ADC', 'STM0', 'STM1']);
			assert.ok(first instanceof PeripheralOptionsList);
			assert.ok(fs.existsSync(await store.jsonPathOf(svdPath)));
			assert.strictEqual(logs.filter((l) => l.includes('converted')).length, 1);
			const again = await new SvdPeripheralStore(path.join(dir, 'cache'), (line) => logs.push(line)).peripherals(svdPath);
			assert.deepStrictEqual(again, first);
			assert.strictEqual(logs.filter((l) => l.includes('converted')).length, 1, 'read from the JSON file');

			// A file that cannot be converted: none, and the view reads it itself next time.
			const bad = path.join(dir, 'BAD.svd');
			fs.writeFileSync(bad, '<device><peripherals>');
			assert.strictEqual(await store.peripherals(bad), undefined);
			assert.strictEqual(store.canProvide(bad), false);
			assert.strictEqual(store.canProvide(svdPath), true);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});
});
