import * as assert from 'assert';
import { PeripheralOptions, PeripheralOptionsList } from '../../peripheralOptions';

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

const peripherals: PeripheralOptions[] = [
	{ name: 'ADC', baseAddress: 0xf0020000, totalLength: 0, description: '' },
	{
		name: 'STM0',
		baseAddress: 0xf0001000,
		totalLength: 0x100,
		description: 'System timer',
		registers: [
			{ name: 'CMP[0]', addressOffset: 0x30, fields: [] },
			{ name: 'TIM0', addressOffset: 0x10, fields: [] },
			{ name: 'CMP[1]', addressOffset: 0x34, fields: [] },
		],
	},
];

suite('Peripheral definitions', () => {
	test('the view builds each child once, sorted', () => {
		const list = PeripheralOptionsList.from(peripherals) as PeripheralOptionsList;
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
});
