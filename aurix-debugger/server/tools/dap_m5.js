// M5 hardware check: symbols, source breakpoints, call stack and source-level
// stepping through the VS Code extension's real debug adapter.
//
// Usage: node tools/dap_m5.js [function] [elf]
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || 'C:/workspace/test/vscode-aurix-tcf-debugger';
const out = (p) => require(path.join(ext, 'out', p));
const { AurixTCFDebugSession } = out('debugProvider.js');

const loc = process.argv[2] || 'C:/workspace/app/bsw/AppData/AppTask/src/AppTask.c:743';
const area = { File: loc.slice(0, loc.lastIndexOf(':')), SLine: Number(loc.slice(loc.lastIndexOf(':') + 1)) };
const elf = process.argv[3] || 'C:/workspace/app/build/bin/app.elf';
const verbose = !!process.env.VERBOSE;
const hex = (n) => '0x' + (n >>> 0).toString(16).padStart(8, '0');

// ---- DAP client ----------------------------------------------------------------------------
class DapClient {
    constructor() {
        this.toAdapter = new PassThrough();
        this.fromAdapter = new PassThrough();
        this.seq = 1; this.pending = new Map(); this.events = []; this.waiters = []; this.buf = Buffer.alloc(0);
        this.fromAdapter.on('data', (d) => this.onData(d));
    }
    onData(d) {
        this.buf = Buffer.concat([this.buf, d]);
        for (;;) {
            const sep = this.buf.indexOf('\r\n\r\n');
            if (sep < 0) return;
            const len = Number(/Content-Length: (\d+)/i.exec(this.buf.subarray(0, sep).toString())[1]);
            if (this.buf.length < sep + 4 + len) return;
            const msg = JSON.parse(this.buf.subarray(sep + 4, sep + 4 + len).toString());
            this.buf = this.buf.subarray(sep + 4 + len);
            if (verbose) console.log('  <-', JSON.stringify(msg).slice(0, 400));
            if (msg.type === 'response') {
                const p = this.pending.get(msg.request_seq);
                if (p) { this.pending.delete(msg.request_seq); msg.success ? p.resolve(msg) : p.reject(new Error(`${msg.command}: ${msg.message}`)); }
            } else if (msg.type === 'event') {
                this.events.push(msg);
                for (const w of [...this.waiters]) if (w.pred(msg)) w.done(msg);
            }
        }
    }
    request(command, args, timeoutMs = 60000) {
        const seq = this.seq++;
        const json = JSON.stringify({ seq, type: 'request', command, arguments: args });
        return new Promise((resolve, reject) => {
            const t = setTimeout(() => reject(new Error(`timeout: ${command}`)), timeoutMs);
            this.pending.set(seq, { resolve: (m) => { clearTimeout(t); resolve(m); }, reject: (e) => { clearTimeout(t); reject(e); } });
            this.toAdapter.write(`Content-Length: ${Buffer.byteLength(json)}\r\n\r\n${json}`);
        });
    }
    waitEvent(pred, what, ms = 20000) {
        const found = this.events.find(pred);
        if (found) { this.events.splice(this.events.indexOf(found), 1); return Promise.resolve(found); }
        return new Promise((resolve, reject) => {
            const w = { pred, done: (e) => { clearTimeout(t); this.waiters.splice(this.waiters.indexOf(w), 1); this.events.splice(this.events.indexOf(e), 1); resolve(e); } };
            const t = setTimeout(() => { this.waiters.splice(this.waiters.indexOf(w), 1); reject(new Error(`timeout waiting for ${what}`)); }, ms);
            this.waiters.push(w);
        });
    }
}

let passed = 0;
const ok = (m) => { passed++; console.log(`  ok  ${m}`); };

async function topFrame(client, threadId) {
    const st = (await client.request('stackTrace', { threadId, startFrame: 0, levels: 20 })).body;
    return st.stackFrames;
}
const where = (f) => `${f.name} ${f.source ? path.basename(f.source.path) : '?'}:${f.line}`;

async function main() {
    const client = new DapClient();
    const session = new AurixTCFDebugSession(false);
    session.start(client.toAdapter, client.fromAdapter);
    await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path' });
    await client.request('launch', { host: '127.0.0.1', port: 1534, program: elf, core: 0, agentFlavor: 'aurix-dbg', liveWatchEnabled: false, debugTCFMessages: verbose });
    await client.waitEvent((e) => e.event === 'initialized', 'initialized');
    ok('launch (MemoryMap.set loads symbols)');

    const bpResp = await client.request('setBreakpoints', { source: { path: area.File }, breakpoints: [{ line: area.SLine }] });
    const bpId = bpResp.body.breakpoints[0].id;
    await client.request('configurationDone', {});
    await client.waitEvent((e) => e.event === 'breakpoint' && e.body.breakpoint.id === bpId && e.body.breakpoint.verified, 'breakpoint verified');
    ok(`source breakpoint ${path.basename(area.File)}:${area.SLine} verified`);

    const threads = (await client.request('threads', {})).body.threads;
    const cpu0 = threads.find((t) => /cpu0/.test(t.name));
    // The cores may still be halted (e.g. by an earlier session): let them run into the breakpoint.
    client.events = client.events.filter((e) => e.event !== 'stopped');
    await client.request('continue', { threadId: cpu0.id }).catch((e) => console.log(`       (continue: ${e.message})`));
    const hit =await client.waitEvent((e) => e.event === 'stopped' && e.body.reason === 'breakpoint', 'breakpoint hit', 30000);
    const tid = hit.body.threadId;
    let frames = await topFrame(client, tid);
    console.log(frames.slice(0, 8).map((f, i) => `       #${i} ${where(f)} ${f.instructionPointerReference}`).join('\n'));
    assert.strictEqual(path.basename(frames[0].source.path).toLowerCase(), path.basename(area.File).toLowerCase());
    assert.strictEqual(frames[0].line, area.SLine);
    assert.ok(frames.length > 1, 'call stack has only one frame');
    ok(`breakpoint hit on thread ${tid}${tid === cpu0.id ? ' (cpu0)' : ''}; call stack ${frames.length} frames, callers resolved: ${frames[1].name}`);

    await client.request('setBreakpoints', { source: { path: area.File }, breakpoints: [] });

    const step = async (cmd, label) => {
        client.events = client.events.filter((e) => e.event !== 'stopped');
        const t0 = Date.now();
        await client.request(cmd, { threadId: tid });
        const ev = await client.waitEvent((e) => e.event === 'stopped' && e.body.threadId === tid, `${cmd} stopped`, 60000);
        const f = await topFrame(client, tid);
        ok(`${label}: ${where(f[0])} (reason ${ev.body.reason}, ${Date.now() - t0} ms)`);
        return f;
    };
    frames = await step('next', 'step over');
    frames = await step('next', 'step over');
    frames = await step('stepIn', 'step into');
    const depthIn = frames.length;
    frames = await step('stepOut', 'step out');
    assert.ok(frames.length <= depthIn, 'step out did not reduce the stack depth');

    client.events = [];
    await client.request('continue', { threadId: tid });
    await client.waitEvent((e) => e.event === 'continued', 'continued');
    ok('continue');
    await client.request('disconnect', {});
    console.log(`\nM5 DAP test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => {
    console.error('\nM5 DAP test FAILED:', e && e.stack ? e.stack : e);
    process.exit(1);
});
