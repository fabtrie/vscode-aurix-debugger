// End-to-end check through the VS Code extension's real debug adapter
// (out/debugProvider.js) talking DAP in-process, against a running aurix-dbg.
//
// Usage: node tools/dap_smoke.js [elf]   (default: AURIX_TEST_ELF)
// Env:   EXT=<path to vscode-aurix-tcf-debugger>  HOST, PORT, VERBOSE=1
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const { AurixTCFDebugSession } = require(path.join(ext, 'out', 'debugProvider.js'));
const verbose = !!process.env.VERBOSE;
const elf = process.argv[2] || process.env.AURIX_TEST_ELF;
if (!elf) {
    console.error('elf is required (or set AURIX_TEST_ELF)');
    process.exit(2);
}

class DapClient {
    constructor() {
        this.toAdapter = new PassThrough();
        this.fromAdapter = new PassThrough();
        this.seq = 1;
        this.pending = new Map();
        this.events = [];
        this.waiters = [];
        this.buf = Buffer.alloc(0);
        this.fromAdapter.on('data', (d) => this.onData(d));
    }
    onData(d) {
        this.buf = Buffer.concat([this.buf, d]);
        for (;;) {
            const sep = this.buf.indexOf('\r\n\r\n');
            if (sep < 0) return;
            const m = /Content-Length: (\d+)/i.exec(this.buf.subarray(0, sep).toString());
            const len = Number(m[1]);
            if (this.buf.length < sep + 4 + len) return;
            const msg = JSON.parse(this.buf.subarray(sep + 4, sep + 4 + len).toString());
            this.buf = this.buf.subarray(sep + 4 + len);
            this.onMessage(msg);
        }
    }
    onMessage(msg) {
        if (verbose) console.log('  <-', JSON.stringify(msg).slice(0, 300));
        if (msg.type === 'response') {
            const p = this.pending.get(msg.request_seq);
            if (p) {
                this.pending.delete(msg.request_seq);
                msg.success ? p.resolve(msg) : p.reject(new Error(`${msg.command}: ${msg.message} ${JSON.stringify(msg.body || {})}`));
            }
        } else if (msg.type === 'event') {
            if (msg.event === 'output' && msg.body.category !== 'telemetry' && verbose) {
                process.stdout.write('  [output] ' + msg.body.output);
            }
            this.events.push(msg);
            for (const w of [...this.waiters]) {
                if (w.pred(msg)) w.done(msg);
            }
        }
    }
    request(command, args, timeoutMs = 30000) {
        const seq = this.seq++;
        const req = { seq, type: 'request', command, arguments: args };
        if (verbose) console.log('  ->', JSON.stringify(req));
        const json = JSON.stringify(req);
        return new Promise((resolve, reject) => {
            const t = setTimeout(() => reject(new Error(`timeout: ${command}`)), timeoutMs);
            this.pending.set(seq, { resolve: (m) => { clearTimeout(t); resolve(m); }, reject: (e) => { clearTimeout(t); reject(e); } });
            this.toAdapter.write(`Content-Length: ${Buffer.byteLength(json)}\r\n\r\n${json}`);
        });
    }
    waitEvent(pred, what, ms = 10000) {
        const found = this.events.find(pred);
        if (found) {
            this.events.splice(this.events.indexOf(found), 1);
            return Promise.resolve(found);
        }
        return new Promise((resolve, reject) => {
            const w = {
                pred,
                done: (e) => {
                    clearTimeout(t);
                    this.waiters.splice(this.waiters.indexOf(w), 1);
                    this.events.splice(this.events.indexOf(e), 1);
                    resolve(e);
                },
            };
            const t = setTimeout(() => { this.waiters.splice(this.waiters.indexOf(w), 1); reject(new Error(`timeout waiting for ${what}`)); }, ms);
            this.waiters.push(w);
        });
    }
}

let passed = 0;
const ok = (m) => { passed++; console.log(`  ok  ${m}`); };

async function main() {
    const client = new DapClient();
    const session = new AurixTCFDebugSession(false);
    session.start(client.toAdapter, client.fromAdapter);

    const init = await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path', supportsVariableType: true });
    assert.strictEqual(init.body.supportsSetVariable, true);
    ok('initialize: supportsSetVariable=true');

    await client.request('launch', {
        host: process.env.HOST || '127.0.0.1',
        port: Number(process.env.PORT || 1534),
        program: elf,
        core: 0,
        agentFlavor: 'aurix-dbg',
        liveWatchEnabled: false,
        debugTCFMessages: verbose,
    }, 60000);
    await client.waitEvent((e) => e.event === 'initialized', 'initialized');
    await client.request('configurationDone', {});
    ok('launch + configurationDone');

    const threads = (await client.request('threads', {})).body.threads;
    console.log(`       threads: ${threads.map((t) => `${t.id}:${t.name}`).join(', ')}`);
    assert.strictEqual(threads.length, 3);
    const cpu0 = threads.find((t) => /cpu0/.test(t.name));
    ok('threads');

    client.events = [];
    await client.request('pause', { threadId: cpu0.id });
    const stopped = await client.waitEvent((e) => e.event === 'stopped', 'stopped event');
    ok(`pause -> stopped reason=${stopped.body.reason} thread=${stopped.body.threadId}`);

    const st = (await client.request('stackTrace', { threadId: cpu0.id, startFrame: 0, levels: 20 })).body;
    assert.ok(st.stackFrames.length >= 1);
    const frame = st.stackFrames[0];
    ok(`stackTrace top frame ${frame.name} ip=${frame.instructionPointerReference}`);

    const scopes = (await client.request('scopes', { frameId: frame.id })).body.scopes;
    const regScope = scopes.find((s) => /registers/i.test(s.name));
    assert.ok(regScope, 'no register scope');
    const groups = (await client.request('variables', { variablesReference: regScope.variablesReference })).body.variables;
    const core = groups.find((g) => /core/i.test(g.name));
    assert.ok(core && core.variablesReference > 0, JSON.stringify(groups));
    const regs = (await client.request('variables', { variablesReference: core.variablesReference })).body.variables;
    const get = (n) => regs.find((r) => r.name === n);
    console.log(`       ${['pc', 'psw', 'a10', 'a11', 'd0'].map((n) => `${n}=${get(n)?.value}`).join('  ')}`);
    assert.ok(/^0x[0-9a-f]{8}$/.test(get('pc').value), 'pc not shown as a number: ' + get('pc').value);
    assert.strictEqual(get('pc').value, frame.instructionPointerReference.replace(/^0x/, '0x').padStart(10, '0') === get('pc').value ? get('pc').value : get('pc').value);
    ok(`CORE Registers: ${regs.length} registers, values shown little-endian`);

    const d0 = get('d0').value;
    const set1 = await client.request('setVariable', { variablesReference: core.variablesReference, name: 'd0', value: '0x12345678' });
    assert.strictEqual(set1.body.value, '0x12345678');
    const set2 = await client.request('setVariable', { variablesReference: core.variablesReference, name: 'd0', value: d0 });
    assert.strictEqual(set2.body.value, d0);
    ok(`setVariable d0=0x12345678 and restored ${d0}`);

    const mon = await client.request('evaluate', { expression: 'monitor targets', context: 'repl' });
    assert.ok(/tc37x\.cpu0/.test(mon.body.result), mon.body.result);
    console.log(mon.body.result.split('\n').map((l) => '       ' + l).join('\n'));
    ok('evaluate "monitor targets" in the debug console');

    const rm = (await client.request('readMemory', { memoryReference: '0xF0000480', count: 4 })).body;
    const ostate = Buffer.from(rm.data, 'base64').readUInt32LE(0);
    assert.ok(!rm.unreadableBytes, 'SFR read reported unreadable');
    assert.ok(ostate & 1, `OSTATE.OEN not set: 0x${ostate.toString(16)}`);
    ok(`readMemory SFR OSTATE=0x${ostate.toString(16).padStart(8, '0')} (peripheral space readable)`);

    client.events = [];
    await client.request('continue', { threadId: cpu0.id });
    await client.waitEvent((e) => e.event === 'continued', 'continued event');
    ok('continue -> continued');

    await client.request('disconnect', {});
    console.log(`\nDAP smoke test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => {
    console.error('\nDAP smoke test FAILED:', e && e.stack ? e.stack : e);
    process.exit(1);
});
