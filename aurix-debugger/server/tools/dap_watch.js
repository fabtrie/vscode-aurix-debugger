// Data breakpoints (watchpoints) through the extension's real debug adapter: on a watch
// expression, with a condition, and the API's in VS Code's Breakpoints view.
//
// Usage: node tools/dap_watch.js [tcf_port] [api_port] [expression] [elf]
// expression (a global) and elf default to AURIX_TEST_WATCH and AURIX_TEST_ELF.
// Runs against the simulator as well (aurix-dbg --adapter sim ...). Nothing writes the variable
// there, so the hit is left to the server's tests; HIT=1 waits for one on a running target.
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const { AurixTCFDebugSession } = require(path.join(ext, 'out', 'debugProvider.js'));

const tcfPort = Number(process.argv[2] || 1534);
const api = `http://127.0.0.1:${Number(process.argv[3] || 8080)}`;
const expr = process.argv[4] || process.env.AURIX_TEST_WATCH;
const elf = process.argv[5] || process.env.AURIX_TEST_ELF;
if (!expr || !elf) {
    console.error('expression and elf are required (or set AURIX_TEST_WATCH and AURIX_TEST_ELF)');
    process.exit(2);
}
const verbose = !!process.env.VERBOSE;

class DapClient {
    constructor() {
        this.toAdapter = new PassThrough();
        this.fromAdapter = new PassThrough();
        this.seq = 1; this.pending = new Map(); this.buf = Buffer.alloc(0); this.events = [];
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
            if (msg.type === 'event') this.events.push(msg);
            if (msg.type === 'response') {
                const p = this.pending.get(msg.request_seq);
                if (p) {
                    this.pending.delete(msg.request_seq);
                    // As VS Code shows it: the error's format with its variables filled in.
                    const err = msg.body && msg.body.error;
                    const text = err ? err.format.replace(/\{(\w+)\}/g, (m, k) => (err.variables && k in err.variables ? err.variables[k] : m)) : msg.message;
                    msg.success ? p.resolve(msg) : p.reject(new Error(`${msg.command}: ${text}`));
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
const watches = async () => (await rest('GET', '/api/v1/watchpoints')).watchpoints;

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

async function main() {
    for (const w of await watches()) await rest('DELETE', '/api/v1/watchpoints', { id: w.id });

    const client = new DapClient();
    const ui = new RecordingUi();
    const session = new AurixTCFDebugSession(false, 'left', ui);
    session.start(client.toAdapter, client.fromAdapter);
    const init = await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path' });
    assert.strictEqual(init.body.supportsDataBreakpoints, true);
    await client.request('launch', { host: '127.0.0.1', port: tcfPort, symbolFiles: [elf], core: 0, agentFlavor: 'aurix-dbg', liveWatchEnabled: false, stopOnEntry: false });
    await client.request('configurationDone', {});
    // The launch hands the ELF to the server, which loads it in the background.
    await until('symbols loaded', async () => {
        const i = await rest('GET', '/api/v1/info');
        return !i.symbols_loading && i.symbol_files.length > 0;
    }, 120000);
    ok('session started (data breakpoints supported), symbols loaded');

    // A watch expression: watched by name, so the server resolves it on every core.
    const info = await client.request('dataBreakpointInfo', { name: expr });
    assert.ok(info.body.dataId, JSON.stringify(info.body));
    const target = JSON.parse(info.body.dataId);
    assert.ok(target.expression && target.expression.startsWith(expr) && target.size > 0, info.body.dataId);
    assert.strictEqual(info.body.canPersist, true);
    ok(`dataBreakpointInfo ${expr}: ${info.body.description}`);

    // Something that is not in memory cannot be watched.
    const none = await client.request('dataBreakpointInfo', { name: '1 + 2' });
    assert.strictEqual(none.body.dataId, null, JSON.stringify(none.body));
    ok(`dataBreakpointInfo 1 + 2: ${none.body.description}`);

    // Set with a condition: the server holds the expression, the access and the condition.
    const set = await client.request('setDataBreakpoints', { breakpoints: [{ dataId: info.body.dataId, accessType: 'write', condition: '1 == 0' }] });
    const bp = set.body.breakpoints[0];
    assert.strictEqual(bp.verified, true, JSON.stringify(bp));
    const mine = await until('planted', async () => (await watches()).find((w) => w.owner === 'client'));
    assert.strictEqual(mine.expression, target.expression);
    assert.strictEqual(mine.access, 'write');
    assert.strictEqual(mine.condition, '1 == 0');
    assert.ok(mine.instances.length > 0 && mine.instances.every((i) => !i.error), JSON.stringify(mine));
    ok(`setDataBreakpoints: ${mine.id} on ${mine.instances.map((i) => `${i.target}@0x${i.address.toString(16)}`).join(', ')}, condition ${mine.condition}`);

    // Changing the access replaces it on the server.
    await client.request('setDataBreakpoints', { breakpoints: [{ dataId: info.body.dataId, accessType: 'readWrite' }] });
    const rw = await until('replaced', async () => {
        const l = (await watches()).filter((w) => w.owner === 'client');
        return l.length === 1 && l[0].access === 'access' ? l[0] : undefined;
    });
    assert.strictEqual(rw.condition, undefined);
    ok('readWrite replaces the write watchpoint');

    if (process.env.HIT) {
        const id = (await client.request('setDataBreakpoints', { breakpoints: [{ dataId: info.body.dataId, accessType: 'write' }] })).body.breakpoints[0].id;
        client.events = [];
        await client.request('continue', { threadId: 1 }).catch(() => undefined);
        const stop = await until('data breakpoint stop', () => client.events.find((e) => e.event === 'stopped' && e.body.reason === 'data breakpoint'), 30000);
        assert.deepStrictEqual(stop.body.hitBreakpointIds, [id], JSON.stringify(stop.body));
        ok(`hit: stopped with reason "data breakpoint" at ${id}`);
    }

    await client.request('setDataBreakpoints', { breakpoints: [] });
    await until('removed', async () => (await watches()).length === 0);
    ok('setDataBreakpoints [] removes it');

    // An API watchpoint shows up in the view, named by what it watches; removing it there
    // removes it on the server.
    const w = await rest('POST', '/api/v1/watchpoints', { expression: expr });
    const shown = await until('API watchpoint shown', () => ui.shown.get(w.id));
    assert.strictEqual(shown.kind, 'function');
    assert.strictEqual(shown.name, `${expr} [write]`);
    ok(`API ${w.id} shown as "${shown.name}"`);
    ui.user({ id: w.id, removed: true });
    await until('removed on the server', async () => !(await watches()).some((x) => x.id === w.id));
    ok('removed in VS Code -> removed on the server');

    // From the Live Watch (its context menu): shown in the view like other clients' watchpoints,
    // and kept when VS Code next sends its own breakpoints (Breakpoints.set replaces the lot).
    const lw = (await client.request('aurix/addDataBreakpoint', { expression: expr, access: 'read' })).body;
    assert.strictEqual(lw.name, `${expr} [read]`);
    const lwShown = await until('Live Watch breakpoint shown', () => ui.shown.get(lw.id));
    assert.strictEqual(lwShown.name, lw.name);
    await client.request('setDataBreakpoints', { breakpoints: [] });
    const lwServer = (await watches()).find((x) => x.id === lw.id);
    assert.ok(lwServer && lwServer.owner === 'client' && lwServer.access === 'read', JSON.stringify(await watches()));
    ok(`Live Watch ${lw.id} shown as "${lw.name}", kept by a later sync`);
    const again = (await client.request('aurix/addDataBreakpoint', { expression: expr, access: 'read' })).body;
    assert.strictEqual(again.id, lw.id);
    await assert.rejects(client.request('aurix/addDataBreakpoint', { expression: 'no_such_variable_xyz' }), /Cannot watch/);
    ok('the same one again is not doubled; an unknown name is refused');
    ui.user({ id: lw.id, removed: true });
    await until('Live Watch breakpoint removed', async () => !(await watches()).some((x) => x.id === lw.id));
    ok('removed in VS Code -> removed on the server');

    // From the editor (its context menu) while a core is halted: evaluated in the selected frame;
    // a global stays a name the server resolves on every core.
    const thread = (await client.request('threads', {})).body.threads[0].id;
    client.events = [];
    await client.request('pause', { threadId: thread });
    await until('paused', () => client.events.find((e) => e.event === 'stopped'));
    const frame = (await client.request('stackTrace', { threadId: thread })).body.stackFrames[0];
    const ed = (await client.request('aurix/addDataBreakpoint', { expression: expr, access: 'write', inFrame: true, frameId: frame.id })).body;
    assert.strictEqual(ed.name, `${expr} [write]`);
    await until('editor breakpoint shown', () => ui.shown.get(ed.id));
    const edServer = (await watches()).find((x) => x.id === ed.id);
    assert.ok(edServer && edServer.expression === expr && edServer.instances.length > 0, JSON.stringify(edServer));
    await assert.rejects(client.request('aurix/addDataBreakpoint', { expression: '1 + 2', inFrame: true, frameId: frame.id }), /not stored in memory/);
    ok(`editor (frame ${frame.name}): ${ed.name} on ${edServer.instances.length} cores; 1 + 2 refused`);
    ui.user({ id: ed.id, removed: true });
    await until('editor breakpoint removed', async () => !(await watches()).some((x) => x.id === ed.id));

    await client.request('disconnect', {});
    console.log(`\nwatchpoint test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => { console.error(e); process.exit(1); });
