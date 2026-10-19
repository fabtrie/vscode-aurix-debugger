// M8 hardware check through the VS Code extension's debug adapter: disassembly
// around the PC and the target reset (ContextReset).
//
// Usage: node tools/dap_m8.js [elf]   (default: AURIX_TEST_ELF)
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const { AurixTCFDebugSession } = require(path.join(ext, 'out', 'debugProvider.js'));
const elf = process.argv[2] || process.env.AURIX_TEST_ELF;
if (!elf) {
    console.error('elf is required (or set AURIX_TEST_ELF)');
    process.exit(2);
}
const verbose = !!process.env.VERBOSE;

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
                if (p) {
                    this.pending.delete(msg.request_seq);
                    const err = msg.body && msg.body.error;
                    const text = err ? err.format.replace(/\{(\w+)\}/g, (m, k) => (err.variables && k in err.variables ? err.variables[k] : m)) : msg.message;
                    msg.success ? p.resolve(msg) : p.reject(new Error(`${msg.command}: ${text}`));
                }
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

async function main() {
    const client = new DapClient();
    const session = new AurixTCFDebugSession(false);
    session.start(client.toAdapter, client.fromAdapter);
    const init = await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path' });
    assert.ok(init.body.supportsDisassembleRequest, 'adapter does not support disassemble');
    await client.request('launch', { host: '127.0.0.1', port: 1534, program: elf, core: 0, agentFlavor: 'aurix-dbg', liveWatchEnabled: false, debugTCFMessages: verbose });
    await client.waitEvent((e) => e.event === 'initialized', 'initialized');
    await client.request('configurationDone', {});
    const threads = (await client.request('threads', {})).body.threads;
    const cpu0 = threads.find((t) => /cpu0/.test(t.name));

    client.events = [];
    await client.request('pause', { threadId: cpu0.id });
    await client.waitEvent((e) => e.event === 'stopped', 'stopped');
    const frame = (await client.request('stackTrace', { threadId: cpu0.id, startFrame: 0, levels: 1 })).body.stackFrames[0];
    const ip = frame.instructionPointerReference;
    ok(`paused in ${frame.name} at ${ip}`);

    const dis = (await client.request('disassemble', { memoryReference: ip, instructionOffset: -4, instructionCount: 12, resolveSymbols: true })).body;
    const lines = dis.instructions || [];
    console.log(lines.map((l) => `       ${l.address}  ${(l.instructionBytes || '').padEnd(12)}  ${l.instruction}`).join('\n'));
    assert.ok(lines.length >= 8, `only ${lines.length} instructions`);
    const at = lines.find((l) => Number(l.address) === Number(ip));
    assert.ok(at && at.instruction && !at.instruction.startsWith('.'), `no decoded instruction at the PC: ${JSON.stringify(at)}`);
    ok(`disassemble: ${lines.length} instructions, at PC: ${at.instruction}`);

    // Toolbar reset (ContextReset), then the application runs again.
    client.events = [];
    await client.request('aurix-tcf/reset', { type: 'reset', suspend: false, core: 0 }, 60000);
    await new Promise((r) => setTimeout(r, 3000));
    const after = (await client.request('threads', {})).body.threads;
    ok(`target reset through the extension (ContextReset.reset), threads after reset: ${after.map((t) => t.name).join(', ')}`);

    await client.request('continue', { threadId: cpu0.id }).catch(() => {});
    await client.request('disconnect', {});
    console.log(`\nM8 DAP test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => {
    console.error('\nM8 DAP test FAILED:', e && e.stack ? e.stack : e);
    process.exit(1);
});
