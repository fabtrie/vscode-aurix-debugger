// Breakpoints of other clients (REST/MCP API) in VS Code's Breakpoints view, through the
// extension's real debug adapter with a recording stand-in for the view.
//
// Usage: node tools/dap_external_bp.js [tcf_port] [api_port] [file:line] [elf]
// file:line and elf default to AURIX_TEST_BP_LOC and AURIX_TEST_ELF.
// Runs against the simulator as well (aurix-dbg --adapter sim ...).
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const { AurixTCFDebugSession } = require(path.join(ext, 'out', 'debugProvider.js'));

const tcfPort = Number(process.argv[2] || 1534);
const api = `http://127.0.0.1:${Number(process.argv[3] || 8080)}`;
const loc = process.argv[4] || process.env.AURIX_TEST_BP_LOC;
const elf = process.argv[5] || process.env.AURIX_TEST_ELF;
if (!loc || !elf) {
    console.error('file:line and elf are required (or set AURIX_TEST_BP_LOC and AURIX_TEST_ELF)');
    process.exit(2);
}
const file = loc.slice(0, loc.lastIndexOf(':'));
const line = Number(loc.slice(loc.lastIndexOf(':') + 1));
const verbose = !!process.env.VERBOSE;

class DapClient {
    constructor() {
        this.toAdapter = new PassThrough();
        this.fromAdapter = new PassThrough();
        this.seq = 1; this.pending = new Map(); this.buf = Buffer.alloc(0);
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
            if (verbose) console.log('  <-', JSON.stringify(msg).slice(0, 300));
            if (msg.type === 'response') {
                const p = this.pending.get(msg.request_seq);
                if (p) {
                    this.pending.delete(msg.request_seq);
                    msg.success ? p.resolve(msg) : p.reject(new Error(`${msg.command}: ${msg.message}`));
                }
            }
        }
    }
    request(command, args, timeoutMs = 120000) {
        const seq = this.seq++;
        const json = JSON.stringify({ seq, type: 'request', command, arguments: args });
        return new Promise((resolve, reject) => {
            const t = setTimeout(() => reject(new Error(`timeout: ${command}`)), timeoutMs);
            this.pending.set(seq, { resolve: (m) => { clearTimeout(t); resolve(m); }, reject: (e) => { clearTimeout(t); reject(e); } });
            this.toAdapter.write(`Content-Length: ${Buffer.byteLength(json)}\r\n\r\n${json}`);
        });
    }
}

/** Stand-in for VS Code's Breakpoints view (VsCodeBreakpointUi). */
class RecordingUi {
    constructor() { this.shown = new Map(); this.listeners = []; this.disposed = false; }
    show(bp) { this.shown.set(bp.id, bp); }
    hide(ids) { ids.forEach((id) => this.shown.delete(id)); }
    onUserChange(l) { this.listeners.push(l); }
    dispose() { this.shown.clear(); this.disposed = true; }
    /** The user removes / enables / disables a shown breakpoint. */
    user(change) {
        if (change.removed) this.shown.delete(change.id);
        this.listeners.forEach((l) => l(change));
    }
}

async function rest(method, p, body) {
    const r = await fetch(api + p, { method, headers: { 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
    const v = await r.json();
    if (!r.ok) throw new Error(`${method} ${p}: ${JSON.stringify(v)}`);
    return v;
}
const serverBps = async () => (await rest('GET', '/api/v1/breakpoints')).breakpoints;

let passed = 0;
const ok = (m) => { passed++; console.log(`  ok  ${m}`); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, pred, ms = 10000) {
    const t0 = Date.now();
    for (;;) {
        const v = await pred();
        if (v) return v;
        if (Date.now() - t0 > ms) throw new Error(`timeout: ${what}`);
        await sleep(50);
    }
}

async function startSession(ui) {
    const client = new DapClient();
    const session = new AurixTCFDebugSession(false, 'left', ui);
    session.start(client.toAdapter, client.fromAdapter);
    const init = await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path' });
    assert.strictEqual(init.body.supportsFunctionBreakpoints, true);
    await client.request('launch', { host: '127.0.0.1', port: tcfPort, program: elf, core: 0, agentFlavor: 'aurix-dbg', liveWatchEnabled: false, stopOnEntry: false });
    await client.request('configurationDone', {});
    return client;
}

async function main() {
    const cpu0 = (await rest('GET', '/api/v1/targets')).targets[0].name;
    for (const b of await serverBps()) await rest('DELETE', `/api/v1/breakpoints/${b.id}`);

    const ui = new RecordingUi();
    const client = await startSession(ui);
    ok('session started (function breakpoints supported)');

    // A source line breakpoint from the API shows up as a source breakpoint.
    const src = await rest('POST', '/api/v1/breakpoints', { file, line, targets: [cpu0] });
    assert.ok(src.instances[0].file && src.instances[0].line, `API reports no source line: ${JSON.stringify(src)}`);
    const shownSrc = await until('source breakpoint shown', () => ui.shown.get(src.id));
    assert.strictEqual(shownSrc.kind, 'source');
    assert.strictEqual(path.basename(shownSrc.file).toLowerCase(), path.basename(file).toLowerCase());
    assert.strictEqual(shownSrc.line, src.line);
    ok(`API ${src.id} ${path.basename(file)}:${line} shown at ${path.basename(shownSrc.file)}:${shownSrc.line} (API: ${path.basename(src.instances[0].file)}:${src.instances[0].line})`);

    // VS Code then sends the file's breakpoints: the line stays the API's (no second breakpoint).
    const r = await client.request('setBreakpoints', { source: { path: shownSrc.file }, breakpoints: [{ line: shownSrc.line }] });
    assert.strictEqual(r.body.breakpoints[0].verified, true);
    assert.strictEqual((await serverBps()).length, 1);
    ok('setBreakpoints with the shown line: verified, no duplicate on the server');

    // An address in the middle of a line: a function breakpoint named by the address.
    const mid = src.instances[0].address + 2;
    const fn = await rest('POST', '/api/v1/breakpoints', { address: mid, targets: [cpu0] });
    const name = `0x${mid.toString(16)}`;
    const shownFn = await until('function breakpoint shown', () => ui.shown.get(fn.id));
    if (shownFn.kind === 'function') {
        assert.strictEqual(shownFn.name, name);
        const fr = await client.request('setFunctionBreakpoints', { breakpoints: [{ name }] });
        assert.strictEqual(fr.body.breakpoints[0].verified, true);
        assert.strictEqual((await serverBps()).length, 2);
        ok(`API ${fn.id} at ${name} (mid-line) shown as function breakpoint ${name}; setFunctionBreakpoints adds nothing`);
    } else {
        ok(`API ${fn.id} at ${name} starts a line: shown as source breakpoint ${path.basename(shownFn.file)}:${shownFn.line}`);
    }

    // Disable / enable in VS Code: the server keeps the breakpoint (still the API's).
    ui.user({ id: fn.id, removed: false, enabled: false });
    await until('disabled on the server', async () => (await serverBps()).find((b) => b.id === fn.id && !b.enabled));
    ui.user({ id: fn.id, removed: false, enabled: true });
    const back = await until('enabled on the server', async () => (await serverBps()).find((b) => b.id === fn.id && b.enabled));
    assert.strictEqual(back.owner, 'api');
    ok('disable/enable in VS Code reaches the server; the breakpoint stays the API\'s');

    // Removed in VS Code -> removed on the server.
    ui.user({ id: src.id, removed: true });
    await until('removed on the server', async () => !(await serverBps()).some((b) => b.id === src.id));
    ok('removed in VS Code -> removed on the server');

    // Removed through the API -> removed from the view.
    await rest('DELETE', `/api/v1/breakpoints/${fn.id}`);
    await until('hidden', () => !ui.shown.has(fn.id));
    ok('removed through the API -> removed from the view');

    // VS Code's own function breakpoints: by address and by file:line.
    const own = await client.request('setFunctionBreakpoints', { breakpoints: [{ name }, { name: `${file}:${line}` }] });
    assert.strictEqual(own.body.breakpoints.length, 2);
    const mine = await until('own function breakpoints planted', async () => {
        const l = (await serverBps()).filter((b) => b.owner === 'client' && b.instances.length > 0 && !b.instances[0].error);
        return l.length === 2 ? l : undefined;
    });
    assert.ok(mine.some((b) => b.address === mid), JSON.stringify(mine));
    assert.ok(mine.some((b) => b.line === line || b.instances[0].line === line), JSON.stringify(mine));
    assert.strictEqual(ui.shown.size, 0, 'own breakpoints must not be shown as external');
    ok(`own function breakpoints ${name} and ${path.basename(file)}:${line} planted`);
    await client.request('setFunctionBreakpoints', { breakpoints: [] });
    await until('own function breakpoints removed', async () => (await serverBps()).length === 0);

    // Breakpoints set before a session starts are shown; the session end removes them from the view.
    await client.request('disconnect', {});
    assert.ok(ui.disposed);
    const early = await rest('POST', '/api/v1/breakpoints', { address: mid, targets: [cpu0] });
    const ui2 = new RecordingUi();
    const client2 = await startSession(ui2);
    await until('existing breakpoint shown', () => ui2.shown.get(early.id));
    ok(`breakpoint set before the session (${early.id}) shown after configurationDone`);
    await client2.request('disconnect', {});
    assert.ok(ui2.disposed && ui2.shown.size === 0);
    assert.ok((await serverBps()).some((b) => b.id === early.id), 'the API breakpoint must survive the VS Code session');
    await rest('DELETE', `/api/v1/breakpoints/${early.id}`);
    ok('session end removes it from the view; it stays on the server');

    console.log(`\nexternal breakpoint test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => { console.error(e); process.exit(1); });
