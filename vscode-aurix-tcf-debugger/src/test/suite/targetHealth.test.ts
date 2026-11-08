import * as assert from 'assert';
import { SmallCommandStamper, TCFLogger } from '../../tcf';
import { TCFClient } from '../../tcfclient';
import { InstanceStatusData } from '../../tcf/breakpoints';
import { GetHealthAurixTargetCommand, TargetHealth, asTargetHealth, parseHealthChangedEvent, parseProbeLostEvent } from '../../tcf/aurixtarget';
import { probeLostReason, targetHealthStatus } from '../../targetHealth';

const b = (s: string) => Buffer.from(s);
const EMPTY = Buffer.alloc(0);

const quietLogger: TCFLogger = {
	sent: () => undefined,
	received: () => undefined,
	log: () => undefined,
	error: () => undefined,
	stdout: () => undefined,
};

/** A TCF client without a socket that records the AurixTarget callbacks. */
class RecordingClient extends TCFClient {
	health: (TargetHealth | null)[] = [];
	lost: string[] = [];

	constructor() {
		super(quietLogger, new SmallCommandStamper());
	}

	event(service: string, event: string, datas: Buffer[]) {
		this.onEvent(service, event, datas);
	}

	protected onTargetHealth(body: TargetHealth | null): void {
		this.health.push(body);
	}
	protected onProbeLost(why: string): void {
		this.lost.push(why);
	}
	protected onBreakpoint(_id: string, _status: InstanceStatusData[]): void { }
	protected onBreakpointRemoved(_ids: string[]): void { }
	protected onBreakpointAdded(_ids: string[]): void { }
	protected onBreakpointChanged(_ids: string[]): void { }
}

suite('Target health (AurixTarget)', () => {
	test('health JSON is validated', () => {
		assert.deepStrictEqual(asTargetHealth({ state: 'unpowered', text: 'Vref 0.1 V', vrefMv: 100 }),
			{ state: 'unpowered', text: 'Vref 0.1 V', vrefMv: 100 });
		assert.deepStrictEqual(asTargetHealth({ state: 'ok', text: 'ok' }), { state: 'ok', text: 'ok' });
		assert.deepStrictEqual(asTargetHealth({ state: 'reset' }), { state: 'reset', text: '' });
		assert.deepStrictEqual(asTargetHealth({ state: 'noDevice', text: 'x', vrefMv: 'high' }), { state: 'noDevice', text: 'x' });
		assert.strictEqual(asTargetHealth(null), null);
		assert.strictEqual(asTargetHealth({ state: 'someFutureState', text: 'x' }), null);
		assert.strictEqual(asTargetHealth('ok'), null);
		assert.strictEqual(asTargetHealth([]), null);
	});

	test('event arguments are parsed, with or without a trailing empty field', () => {
		assert.deepStrictEqual(parseHealthChangedEvent([b('{"state":"reset","text":"target reset"}'), EMPTY]),
			{ state: 'reset', text: 'target reset' });
		assert.strictEqual(parseHealthChangedEvent([b('null')]), null);
		assert.strictEqual(parseHealthChangedEvent([]), null);
		assert.strictEqual(parseProbeLostEvent([b('"BlueBox USB disconnected"'), EMPTY]), 'BlueBox USB disconnected');
		assert.strictEqual(parseProbeLostEvent([b('null')]), '');
		assert.throws(() => parseProbeLostEvent([b('not json')]));
	});

	test('the client dispatches AurixTarget events', () => {
		const c = new RecordingClient();
		c.event('AurixTarget', 'healthChanged', [b('{"state":"noDebugAccess","text":"locked","vrefMv":3300}'), EMPTY]);
		c.event('AurixTarget', 'healthChanged', [b('null'), EMPTY]);
		c.event('AurixTarget', 'healthChanged', [b('{bad')]); // dropped
		c.event('AurixTarget', 'probeLost', [b('"miniWiggler gone"'), EMPTY]);
		c.event('AurixTarget', 'probeLost', [b('{bad')]); // still lost, no reason
		c.event('AurixTarget', 'somethingNew', [b('1')]); // ignored
		assert.deepStrictEqual(c.health, [{ state: 'noDebugAccess', text: 'locked', vrefMv: 3300 }, null]);
		assert.deepStrictEqual(c.lost, ['miniWiggler gone', '']);
	});

	test('getHealth is only sent to servers with the AurixTarget service', async () => {
		const c = new RecordingClient();
		c.supportedServices = ['Locator', 'RunControl'];
		assert.strictEqual(await c.getTargetHealth(), undefined);
		c.supportedServices = null;
		assert.strictEqual(await c.getTargetHealth(), undefined);
	});

	test('getHealth command wire format and result', async () => {
		const cmd = new GetHealthAurixTargetCommand();
		assert.strictEqual(cmd.toBuffer('AurixTarget/1').toString('latin1'), 'C\0AurixTarget/1\0AurixTarget\0getHealth\0\x03\x01');
		assert.deepStrictEqual(await cmd.resultPromise([EMPTY, b('{"state":"unpowered","text":"no Vref","vrefMv":12}'), EMPTY]),
			{ state: 'unpowered', text: 'no Vref', vrefMv: 12 });
		assert.strictEqual(await cmd.resultPromise([EMPTY, b('null')]), null);
	});

	test('status bar text: hidden for ok or none, labelled otherwise', () => {
		assert.strictEqual(targetHealthStatus(undefined), undefined);
		assert.strictEqual(targetHealthStatus(null), undefined);
		assert.strictEqual(targetHealthStatus({ state: 'ok', text: 'fine' }), undefined);
		assert.deepStrictEqual(targetHealthStatus({ state: 'unpowered', text: 'Target Vref too low', vrefMv: 120 }),
			{ text: '$(debug-disconnect) Target unpowered', tooltip: 'Target Vref too low\nVref: 0.12 V' });
		assert.strictEqual(targetHealthStatus({ state: 'reset', text: 'r' })?.text, '$(sync) Target reset');
		assert.strictEqual(targetHealthStatus({ state: 'inReset', text: 'old name' }), undefined);
		assert.strictEqual(targetHealthStatus({ state: 'probeLost', text: 'p' })?.text, '$(debug-disconnect) Probe lost');
		assert.strictEqual(targetHealthStatus({ state: 'noDebugAccess', text: 'd' })?.text, '$(warning) No debug access');
		assert.strictEqual(targetHealthStatus({ state: 'noDevice', text: 'n' })?.text, '$(warning) No device');
		assert.deepStrictEqual(targetHealthStatus({ state: 'noDevice', text: '' }), { text: '$(warning) No device', tooltip: 'No device' });
	});

	test('probe lost reason', () => {
		assert.strictEqual(probeLostReason({ why: 'BlueBox USB disconnected' }), 'BlueBox USB disconnected');
		assert.strictEqual(probeLostReason({}), 'unknown reason');
		assert.strictEqual(probeLostReason(undefined), 'unknown reason');
	});
});
