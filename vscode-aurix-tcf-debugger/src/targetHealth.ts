// Presentation of the probe/target health (AurixTarget service) in the status bar. No vscode
// import, so the tests can check it directly.
import { TargetHealth, asTargetHealth } from './tcf/aurixtarget';

export interface TargetHealthStatus {
	text: string;
	tooltip: string;
}

const LABELS: Record<Exclude<TargetHealth['state'], 'ok'>, string> = {
	unpowered: '$(debug-disconnect) Target unpowered',
	// A latch: the target reset since the probe last connected (shown while the debugger reconnects).
	reset: '$(sync) Target reset',
	noDebugAccess: '$(warning) No debug access',
	noDevice: '$(warning) No device',
	probeLost: '$(debug-disconnect) Probe lost',
};

/** What the status bar item shows for a health event body; undefined hides it (ok or none). */
export function targetHealthStatus(body: unknown): TargetHealthStatus | undefined {
	const health = asTargetHealth(body);
	if (!health || health.state === 'ok') {
		return undefined;
	}
	const lines: string[] = [];
	if (health.text) {
		lines.push(health.text);
	}
	if (health.vrefMv !== undefined) {
		lines.push(`Vref: ${(health.vrefMv / 1000).toFixed(2)} V`);
	}
	return { text: LABELS[health.state], tooltip: lines.join('\n') || LABELS[health.state].replace(/^\$\([^)]*\)\s*/, '') };
}

/** The `why` of an `aurix/probeLost` event body. */
export function probeLostReason(body: unknown): string {
	const why = (body as { why?: unknown } | undefined)?.why;
	return typeof why === 'string' && why.trim() !== '' ? why : 'unknown reason';
}
