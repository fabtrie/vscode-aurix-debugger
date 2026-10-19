// End-to-end check of aurix-dbg's TCF frontend with the VS Code extension's
// own compiled TCF client (same parsers and JSON validators as the debugger UI).
//
// Usage: node tools/tcf_smoke.js [host] [port]
// Env:   EXT=<path to vscode-aurix-tcf-debugger>  VERBOSE=1
'use strict';
const path = require('path');
const assert = require('assert');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const out = (p) => require(path.join(ext, 'out', p));
const { TCFClient } = out('tcfclient.js');
const { SmallCommandStamper } = out('tcf.js');
const rc = out('tcf/runcontrol.js');
const mem = out('tcf/memory.js');
const reg = out('tcf/registers.js');
const bps = out('tcf/breakpoints.js');
const st = out('tcf/stacktrace.js');
const utils = out('tcf/tcfutils.js');

const host = process.argv[2] || 'localhost';
const port = Number(process.argv[3] || 1534);
const verbose = !!process.env.VERBOSE;

const logger = {
    sent: (m) => verbose && console.log('  ->', m.replace(/\0/g, ' | ')),
    received: (m) => verbose && console.log('  <-', m),
    log: (m) => verbose && console.log('  log', m),
    error: (m) => console.error('  client error:', m),
    stdout: () => {},
};

class RawCommand extends utils.SimpleCommand {
    constructor(service, command, args, parser) {
        super();
        this.s = service; this.c = command; this.a = args; this.p = parser;
    }
    service() { return this.s; }
    command() { return this.c; }
    debugDescription(i) { return `${this.s}/${i}`; }
    arguments() { return undefined; }
    toBuffer(token) { return utils.toBuffer(['C', token, this.s, this.c, ...this.a.map((x) => JSON.stringify(x))]); }
    result(r, ok, err) { this.p(r, ok, err); }
}
const emptyParser = (r, ok, err) => utils.parseEmptyResponse(r, ok, err);
const jsonParser = (r, ok, err) => utils.parseResponse(r, ok, err, (x) => x);

class Client extends TCFClient {
    constructor() {
        super(logger, new SmallCommandStamper());
        this.events = [];
        this.waiters = [];
    }
    onBreakpoint(id, status) { this.push({ type: 'bphit', id, status }); }
    onBreakpointRemoved(ids) { this.push({ type: 'bpremoved', ids }); }
    onBreakpointAdded(ids) { this.push({ type: 'bpadded', ids }); }
    onBreakpointChanged(ids) { this.push({ type: 'bpchanged', ids }); }
    onRunControlContextSuspended(info) { this.push({ type: 'suspended', info }); }
    onRunControlContextResumed(id) { this.push({ type: 'resumed', id }); }
    push(e) {
        if (verbose) console.log('  event', JSON.stringify(e));
        this.events.push(e);
        for (const w of [...this.waiters]) {
            if (w.pred(e)) { w.done(e); }
        }
    }
    waitFor(pred, what, ms = 5000) {
        const found = this.events.find(pred);
        if (found) {
            this.events.splice(this.events.indexOf(found), 1);
            return Promise.resolve(found);
        }
        return new Promise((resolve, reject) => {
            const w = {
                pred,
                done: (e) => {
                    clearTimeout(timer);
                    this.waiters.splice(this.waiters.indexOf(w), 1);
                    this.events.splice(this.events.indexOf(e), 1);
                    resolve(e);
                },
            };
            const timer = setTimeout(() => {
                this.waiters.splice(this.waiters.indexOf(w), 1);
                reject(new Error(`timeout waiting for ${what}`));
            }, ms);
            this.waiters.push(w);
        });
    }
}

const hex = (n) => '0x' + (n >>> 0).toString(16).padStart(8, '0');
let passed = 0;
function ok(msg) { passed++; console.log(`  ok  ${msg}`); }

async function main() {
    const c = new Client();
    await c.connect(host, port);
    ok(`handshake, services: ${c.supportedServices.join(', ')}`);

    const threads = await c.getThreads();
    assert.strictEqual(threads.length, 3, 'expected 3 cores');
    for (const t of threads) {
        console.log(`       ${t.info.ID} ${t.info.Name} ${t.state.suspended ? 'suspended pc=' + hex(t.state.pc) + ' ' + t.state.lastStateReason : 'running'}`);
    }
    ok('RunControl tree + getState for 3 cores');
    const ctx0 = threads.find((t) => /\.C0$/.test(t.info.ID)).info.ID;

    // Memory
    let m = await c.sendCommand(new mem.GetMemoryCommand(ctx0, 0x80217cc0, 1, 64, 0));
    assert.strictEqual(Buffer.from(m.base64, 'base64').length, 64);
    assert.strictEqual(m.errorAddresses, null);
    ok('Memory.get 64 bytes of PFLASH');
    // Note: no read of erased PFLASH here - the ECC alarm can make the
    // application's SMU reset the device. Partial reads are covered offline.

    // Halt
    let st0 = await c.sendCommand(new rc.GetStateRunControlCommand(ctx0));
    if (!st0.suspended) {
        await c.pause(ctx0);
        const ev = await c.waitFor((e) => e.type === 'suspended' && e.info.id === ctx0, 'contextSuspended');
        ok(`suspend -> contextSuspended reason=${ev.info.reason} pc=${hex(ev.info.pc)}`);
        await new Promise((r) => setTimeout(r, 100));
        const others = c.events.filter((e) => e.type === 'suspended').map((e) => e.info.id);
        console.log(`       other cores suspended with it (TL1): ${others.join(', ') || 'none'}`);
    }
    st0 = await c.sendCommand(new rc.GetStateRunControlCommand(ctx0));
    assert.ok(st0.suspended);
    ok(`getState suspended pc=${hex(st0.pc)} reason=${st0.lastStateReason}`);
    try {
        await c.pause(ctx0); // already stopped -> error 10 is swallowed by the client
        ok('suspend on halted core tolerated (ALREADY_STOPPED)');
    } catch (e) {
        throw new Error('suspend on halted core failed: ' + JSON.stringify(e));
    }

    // Registers
    const groups = await c.sendCommand(new reg.GetChildrenRegistersCommand(ctx0));
    const groupCtx = await Promise.all(groups.map((g) => c.sendCommand(new reg.GetContextRegistersCommand(g))));
    const core = groupCtx.find((g) => /^core( registers)?$/i.test(g.Name));
    assert.ok(core, 'no core register group');
    const regIds = await c.sendCommand(new reg.GetChildrenRegistersCommand(core.ID));
    const regCtx = await Promise.all(regIds.map((r) => c.sendCommand(new reg.GetContextRegistersCommand(r))));
    const pcReg = regCtx.find((r) => r.Name === 'pc');
    const d0Reg = regCtx.find((r) => r.Name === 'd0');
    const pcVal = (await c.sendCommand(new reg.GetRegistersCommand(pcReg.ID))).readUInt32LE(0);
    assert.strictEqual(pcVal, st0.pc >>> 0);
    ok(`Registers: ${regIds.length} core registers, pc=${hex(pcVal)} matches getState`);
    const d0 = (await c.sendCommand(new reg.GetRegistersCommand(d0Reg.ID))).readUInt32LE(0);
    const setReg = (id, v) => {
        const b = Buffer.alloc(4); b.writeUInt32LE(v >>> 0);
        return c.sendCommand(new RawCommand('Registers', 'set', [id, b.toString('base64')], emptyParser));
    };
    await setReg(d0Reg.ID, 0x12345678);
    const d0new = (await c.sendCommand(new reg.GetRegistersCommand(d0Reg.ID))).readUInt32LE(0);
    await setReg(d0Reg.ID, d0);
    assert.strictEqual(d0new, 0x12345678);
    ok(`Registers.set d0=0x12345678 read back, restored ${hex(d0)}`);

    // Memory write round trip (same bytes back) on the CSA area of CPU0
    const ramAddr = 0x70007c00;
    const orig = await c.sendCommand(new mem.GetMemoryCommand(ctx0, ramAddr, 1, 8, 0));
    const origBuf = Buffer.from(orig.base64, 'base64');
    const errs = await c.sendCommand(new mem.SetMemoryCommand(ctx0, ramAddr, 1, 8, origBuf.toString('base64'), 0));
    assert.ok(errs === null || errs.length === 0);
    ok('Memory.set (unchanged bytes) accepted');

    // StackTrace
    const frames = await c.sendCommand(new st.GetChildrenStackTraceCommand(ctx0));
    const fctx = await c.sendCommand(new st.GetContextStackTraceCommand(frames));
    // getChildren lists the outermost frame first; the top frame is Index 0.
    const top = fctx.find((f) => f.Index === 0);
    assert.ok(top && top.TopFrame, JSON.stringify(fctx.map((f) => f.ID)));
    assert.strictEqual(top.IP, st0.pc);
    ok(`StackTrace ${fctx.length} frames, top IP=${hex(top.IP)} FP=${hex(top.FP)} RP=${hex(top.RP)}`);

    // Instruction breakpoint at the current PC, then continue until it hits
    const bpId = 'aurix-smoke-1';
    await c.setBreakpointsList([{ ID: bpId, Enabled: true, Address: hex(st0.pc), Location: hex(st0.pc), BreakpointType: 'Hardware', ContextIds: [ctx0] }]);
    await c.waitFor((e) => e.type === 'bpadded' && e.ids.includes(bpId), 'Breakpoints contextAdded');
    const status = await c.sendCommand(new bps.GetStatusBreakpointsCommand(bpId));
    assert.ok(status.Instances && status.Instances.length === 1 && !status.Instances[0].Error, JSON.stringify(status));
    ok(`breakpoint verified: ${JSON.stringify(status.Instances[0])}`);
    c.events = [];
    await c.continue(ctx0);
    await c.waitFor((e) => e.type === 'resumed' && e.id === ctx0, 'contextResumed');
    ok('continue -> contextResumed');
    const hit = await c.waitFor((e) => e.type === 'suspended' && e.info.id === ctx0, 'breakpoint hit', 8000);
    assert.ok(/breakpoint/i.test(hit.info.reason), 'reason ' + hit.info.reason);
    assert.strictEqual(hit.info.pc, st0.pc);
    assert.ok(Array.isArray(hit.info.data.BPs) && hit.info.data.BPs.includes(bpId));
    const hitStatus = c.events.find((e) => e.type === 'bphit');
    ok(`breakpoint hit: reason=${hit.info.reason} pc=${hex(hit.info.pc)} BPs=${hit.info.data.BPs} status-event=${!!hitStatus}`);
    await c.setBreakpointsList([]);
    await c.waitFor((e) => e.type === 'bpremoved' && e.ids.includes(bpId), 'Breakpoints contextRemoved');
    ok('breakpoint removed');

    // Source breakpoint in a file without line information (symbols may be loaded by an
    // earlier session): not verified, error status
    await c.setBreakpointsList([{ ID: 'aurix-smoke-2', Enabled: true, File: 'no/such/dir/aurix_smoke_missing.c', Line: 10 }]);
    const st2 = await c.sendCommand(new bps.GetStatusBreakpointsCommand('aurix-smoke-2'));
    assert.ok(st2.Error, JSON.stringify(st2));
    ok(`File/Line breakpoint reports: ${st2.Error}`);
    await c.setBreakpointsList([]);

    // Single instruction step
    c.events = [];
    await c.nextInstruction(ctx0);
    const stepEv = await c.waitFor((e) => e.type === 'suspended' && e.info.id === ctx0, 'step contextSuspended');
    assert.ok(/step/i.test(stepEv.info.reason), 'reason ' + stepEv.info.reason);
    ok(`instruction step: ${hex(st0.pc)} -> ${hex(stepEv.info.pc)} reason=${stepEv.info.reason}`);

    // Tcl console through TCF
    const tcl = await c.sendCommand(new RawCommand('Tcl', 'eval', ['tc3xx_stm_status'], jsonParser));
    console.log(tcl.split('\n').map((l) => '       ' + l).join('\n'));
    ok('Tcl.eval tc3xx_stm_status');

    // Resume and leave the target running
    c.events = [];
    await c.continue(ctx0);
    await c.waitFor((e) => e.type === 'resumed' && e.id === ctx0, 'final contextResumed');
    ok('final continue');
    c.disconnect();
    console.log(`\nTCF smoke test passed (${passed} checks)`);
}

main().catch((e) => {
    console.error('\nTCF smoke test FAILED:', e && e.stack ? e.stack : JSON.stringify(e));
    process.exit(1);
});
