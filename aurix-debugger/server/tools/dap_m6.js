// M6 hardware check: local variables, expressions, assignments and live watch
// through the VS Code extension's real debug adapter.
//
// Usage: node tools/dap_m6.js [file:line] [elf]
// file:line and elf default to AURIX_TEST_BP_LOC and AURIX_TEST_ELF. The variables come from
// the environment too, all required:
//   AURIX_TEST_STACK_LOCAL  local on the stack at file:line, in SMPU-protected RAM
//   AURIX_TEST_REG_LOCAL    local held in a data register at file:line
//   AURIX_TEST_REG          that register (default d8)
//   AURIX_TEST_GLOBAL       integer global in debugger-writable RAM
//   AURIX_TEST_ENUM         4-byte enum global
//   AURIX_TEST_STRUCT       struct global
//   AURIX_TEST_COUNTER      integer that changes while the target runs
'use strict';
const path = require('path');
const assert = require('assert');
const { PassThrough } = require('stream');

const ext = process.env.EXT || path.resolve(__dirname, '../../../vscode-aurix-tcf-debugger');
const { AurixTCFDebugSession } = require(path.join(ext, 'out', 'debugProvider.js'));

const loc = process.argv[2] || process.env.AURIX_TEST_BP_LOC;
const elf = process.argv[3] || process.env.AURIX_TEST_ELF;
const file = loc.slice(0, loc.lastIndexOf(':'));
const line = Number(loc.slice(loc.lastIndexOf(':') + 1));
const verbose = !!process.env.VERBOSE;
const COUNTER = process.env.AURIX_TEST_COUNTER;
const STRUCT = process.env.AURIX_TEST_STRUCT;
const ENUM = process.env.AURIX_TEST_ENUM;
const GLOBAL = process.env.AURIX_TEST_GLOBAL;
const STACK_LOCAL = process.env.AURIX_TEST_STACK_LOCAL;
const REG_LOCAL = process.env.AURIX_TEST_REG_LOCAL;
const REG = process.env.AURIX_TEST_REG || 'd8';
if (!loc || !elf || !COUNTER || !STRUCT || !ENUM || !GLOBAL || !STACK_LOCAL || !REG_LOCAL) {
    console.error('file:line, elf and the AURIX_TEST_* variables listed in the header are required');
    process.exit(2);
}

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
                    // Error messages are templates with {variables}, like VS Code shows them.
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
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
    const client = new DapClient();
    const session = new AurixTCFDebugSession(false);
    session.start(client.toAdapter, client.fromAdapter);
    const init = await client.request('initialize', { adapterID: 'aurix-tcf', linesStartAt1: true, columnsStartAt1: true, pathFormat: 'path' });
    assert.strictEqual(init.body.supportsSetExpression, true);
    await client.request('launch', { host: '127.0.0.1', port: 1534, program: elf, core: 0, agentFlavor: 'aurix-dbg', liveWatchEnabled: false, debugTCFMessages: verbose });
    await client.waitEvent((e) => e.event === 'initialized', 'initialized');
    const bp = await client.request('setBreakpoints', { source: { path: file }, breakpoints: [{ line }] });
    await client.request('configurationDone', {});
    await client.waitEvent((e) => e.event === 'breakpoint' && e.body.breakpoint.id === bp.body.breakpoints[0].id && e.body.breakpoint.verified, 'breakpoint verified');
    const threads = (await client.request('threads', {})).body.threads;
    const cpu0 = threads.find((t) => /cpu0/.test(t.name));
    client.events = client.events.filter((e) => e.event !== 'stopped');
    await client.request('continue', { threadId: cpu0.id }).catch(() => {});
    const hit = await client.waitEvent((e) => e.event === 'stopped' && e.body.reason === 'breakpoint', 'breakpoint hit', 30000);
    const tid = hit.body.threadId;
    await client.request('setBreakpoints', { source: { path: file }, breakpoints: [] });
    ok(`stopped at ${path.basename(file)}:${line} on thread ${tid}`);

    const frames = (await client.request('stackTrace', { threadId: tid, startFrame: 0, levels: 20 })).body.stackFrames;
    const scopes = (await client.request('scopes', { frameId: frames[0].id })).body.scopes;
    const localsRef = scopes.find((s) => s.name === 'Locals').variablesReference;
    const locals = async () => (await client.request('variables', { variablesReference: localsRef })).body.variables;
    let vars = await locals();
    console.log(`       locals: ${vars.map((v) => `${v.name}=${v.value}`).join(', ')}`);
    const get = (list, n) => list.find((v) => v.name === n);
    assert.ok(get(vars, STACK_LOCAL) && get(vars, REG_LOCAL), 'expected locals missing');
    assert.ok(vars.every((v) => v.value !== ''), 'a local has no value');
    ok(`Locals of frame 0: ${vars.length} variables with values`);

    // Stack variable: the application's SMPU does not let the debugger write CPU0's
    // stack (below 0x7000B400), so the write must be reported instead of silently ignored.
    const orig = get(vars, STACK_LOCAL).value;
    const denied = await client.request('setVariable', { variablesReference: localsRef, name: STACK_LOCAL, value: '0x5A' }).then(() => null, (e) => e.message);
    assert.ok(denied && /write-protected/.test(denied), `stack write not reported: ${denied}`);
    vars = await locals();
    assert.strictEqual(get(vars, STACK_LOCAL).value, orig);
    ok(`setVariable stack local ${STACK_LOCAL} reports the SMPU protection: ${denied.replace(/^setVariable: /, '')}`);

    // Global in a debugger-writable SMPU region: write and restore.
    const g0 = (await client.request('evaluate', { expression: GLOBAL, frameId: frames[0].id, context: 'watch' })).body.result;
    const g1 = String((Number(g0) + 1) >>> 0);
    assert.strictEqual((await client.request('setExpression', { expression: GLOBAL, value: g1, frameId: frames[0].id })).body.value, g1);
    assert.strictEqual((await client.request('evaluate', { expression: GLOBAL, frameId: frames[0].id, context: 'watch' })).body.result, g1);
    assert.strictEqual((await client.request('setExpression', { expression: GLOBAL, value: g0, frameId: frames[0].id })).body.value, g0);
    ok(`setExpression global ${GLOBAL}: ${g0} -> ${g1} -> ${g0}`);

    // Register variable
    const hw = get(vars, REG_LOCAL).value;
    const flipped = hw === '0' ? '1' : '0';
    assert.strictEqual((await client.request('setVariable', { variablesReference: localsRef, name: REG_LOCAL, value: flipped })).body.value, flipped);
    const regs = (await client.request('scopes', { frameId: frames[0].id })).body.scopes.find((s) => /registers/i.test(s.name));
    const groups = (await client.request('variables', { variablesReference: regs.variablesReference })).body.variables;
    const core = (await client.request('variables', { variablesReference: groups.find((g) => /core/i.test(g.name)).variablesReference })).body.variables;
    assert.strictEqual(Number(core.find((r) => r.name === REG).value) & 0xff, Number(flipped), `${REG} does not hold the new value`);
    assert.strictEqual((await client.request('setVariable', { variablesReference: localsRef, name: REG_LOCAL, value: hw })).body.value, hw);
    ok(`setVariable register local ${REG_LOCAL} (${REG.toUpperCase()}): ${hw} -> ${flipped} -> ${hw}`);

    if (frames.length > 1) {
        const s1 = (await client.request('scopes', { frameId: frames[1].id })).body.scopes.find((s) => s.name === 'Locals');
        const v1 = (await client.request('variables', { variablesReference: s1.variablesReference })).body.variables;
        ok(`Locals of caller ${frames[1].name}: ${v1.map((v) => `${v.name}=${v.value}`).join(', ') || '(none)'}`);
    }

    const evaluate = async (expression, context = 'watch') => (await client.request('evaluate', { expression, frameId: frames[0].id, context })).body;
    const e1 = await evaluate(ENUM);
    assert.ok(/^[A-Za-z_]\w*$/.test(e1.result), `enum shown as ${e1.result}`);
    const e2 = await evaluate(`sizeof(${ENUM}) * 2 + (${ENUM} == ${e1.result})`);
    assert.strictEqual(e2.result, '9');
    const e3 = await evaluate(`*(uint8*)&${STACK_LOCAL}`);
    assert.strictEqual(e3.result, orig);
    ok(`evaluate: ${ENUM} = ${e1.result}; sizeof/compare = ${e2.result}; *(uint8*)&${STACK_LOCAL} = ${e3.result}`);

    const se = await client.request('setExpression', { expression: ENUM, value: e1.result, frameId: frames[0].id });
    assert.strictEqual(se.body.value, e1.result);
    ok(`setExpression ${ENUM} = ${e1.result} (enumerator name)`);

    const st = await evaluate(STRUCT);
    assert.ok(st.variablesReference > 0, `struct has no children: ${JSON.stringify(st)}`);
    const fields = (await client.request('variables', { variablesReference: st.variablesReference })).body.variables;
    console.log(`       ${STRUCT} = ${st.result}`);
    console.log(`       fields: ${fields.map((f) => `${f.name}=${f.value}`).join(', ')}`);
    const field = fields.find((f) => /^-?\d+$/.test(f.value));
    assert.ok(field, 'no numeric struct field');
    const sf = await client.request('setVariable', { variablesReference: st.variablesReference, name: field.name, value: field.value });
    assert.strictEqual(sf.body.value, field.value);
    ok(`struct ${STRUCT}: ${fields.length} fields, setVariable ${field.name} = ${field.value}`);

    // Live watch while running
    await client.request('continue', { threadId: tid });
    await client.waitEvent((e) => e.event === 'continued', 'continued');
    await sleep(300);
    const lw = async () => (await client.request('aurix-tcf/liveWatchEvaluate', { expressions: [COUNTER, ENUM, `${STRUCT}.${field.name}`] })).body.values;
    const first = await lw();
    await sleep(700);
    const second = await lw();
    console.log(`       running: ${first.map((v) => `${v.expression}=${v.available ? v.result : `n/a (${v.error})`}`).join(', ')}`);
    console.log(`       +700 ms: ${second.map((v) => `${v.expression}=${v.available ? v.result : `n/a (${v.error})`}`).join(', ')}`);
    assert.ok(first.every((v) => v.available) && second.every((v) => v.available), 'live watch values not available while running');
    const counterChanged = first[0].result !== second[0].result;
    ok(`live watch while running (${COUNTER} ${counterChanged ? 'changes' : 'did not change'})`);
    const wr = await client.request('aurix-tcf/liveWatchWrite', { expression: ENUM, value: second[1].result });
    assert.strictEqual(wr.body.result, second[1].result);
    ok(`liveWatchWrite ${ENUM} = ${wr.body.result} while running`);

    await client.request('disconnect', {});
    console.log(`\nM6 DAP test passed (${passed} checks)`);
    process.exit(0);
}

main().catch((e) => {
    console.error('\nM6 DAP test FAILED:', e && e.stack ? e.stack : e);
    process.exit(1);
});
