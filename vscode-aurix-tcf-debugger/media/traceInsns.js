// Trace panel webview, Instructions tab: every executed instruction with its source line, what it
// wrote and the registers rebuilt from the halt (server: AurixTrace insns / regs / find), and the
// replay of a row in the debugger. Rows are loaded in pages and drawn as a virtual list.
(function () {
	const vscode = window.traceVscode;
	const $ = (id) => document.getElementById(id);
	const ROW_H = 19;
	const PAGE = 2000;

	let result; // the program recording (trace read)
	let total = 0;
	let rows = []; // trace rows by index
	let loaded = 0;
	let loading = false;
	let funcs = [];
	let files = [];
	let endNs;
	let summary;
	let sources = {}; // file -> lines
	let display = []; // what the list shows: {k: 'fn'|'src'|'row'|'halt', i?, d, ...}
	let rowPos = new Map(); // trace row -> display index
	let sel = -1;
	let replayAt; // the row the debugger shows, while a replay is active
	let pick = 'd15';
	let regsFor = -1;
	let regs; // the last regs answer
	let showSrc = true;
	let showEff = true;
	let callsOnly = false;
	let active = false;
	let regsTimer;
	let wide = 0; // widest row drawn so far: the horizontal scroll width only grows
	let wholeHistory = true; // reverse debugging: every action of the history, not only the last

	/** Reverse debugging shows the whole history: the rows carry their action (Act) and row (R). */
	function historyMode() {
		return !!window.aurixReverse && wholeHistory;
	}
	/** The scope argument of insns / regs / find. */
	function scope() {
		return window.aurixReverse ? (wholeHistory ? 'history' : 'action') : undefined;
	}

	function esc(s) {
		return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
	}
	function hex(n, w = 8) {
		return n === null || n === undefined ? '' : '0x' + (Number(n) >>> 0).toString(16).toUpperCase().padStart(w, '0');
	}
	function fmtNs(ns) {
		if (ns === null || ns === undefined) return '';
		const a = Math.abs(ns);
		if (a >= 1e6) return (ns / 1e6).toFixed(3) + ' ms';
		if (a >= 1e3) return (ns / 1e3).toFixed(3) + ' µs';
		return Math.round(ns) + ' ns';
	}
	function baseName(p) {
		return String(p || '').split(/[\\/]/).pop();
	}
	/** A register value: exact, from memory (≈) or unknown; partly known values show ? digits. */
	function val(v, mark, known) {
		if (v === null || v === undefined) return '<span class="v-unk">········</span>';
		let s = hex(v);
		if (known !== null && known !== undefined) {
			const digits = s.slice(2).split('');
			for (let n = 0; n < 8; n++) {
				const m = (known >>> (4 * (7 - n))) & 0xf;
				if (m !== 0xf) digits[n] = '?';
			}
			s = '0x' + digits.join('');
		}
		return mark === 'mem' ? `<span class="v-mem">≈${s}</span>` : `<span class="v-exact">${s}</span>`;
	}

	// ---- loading ------------------------------------------------------------------------------

	function reset(r) {
		result = r;
		rows = [];
		total = 0;
		loaded = 0;
		loading = false;
		display = [];
		rowPos = new Map();
		sel = -1;
		regs = undefined;
		regsFor = -1;
		replayAt = undefined;
		summary = undefined;
		sources = {};
		$('insnRows').innerHTML = '';
		$('insnRegs').innerHTML = '';
		$('insnSummary').textContent = '';
		if (active && ((r && r.Kind === 'program') || historyMode())) load();
	}

	function load() {
		if (loading || (total && loaded >= total)) return;
		loading = true;
		$('insnSummary').textContent = 'Loading the instructions…';
		vscode.postMessage({ type: 'insns', from: loaded, count: PAGE, scope: scope() });
	}

	function onPage(m) {
		loading = false;
		if (m.error) {
			$('insnSummary').innerHTML = `<span class="bad">${esc(m.error)}</span>`;
			return;
		}
		const r = m.result || {};
		total = r.Total || 0;
		funcs = r.Functions || funcs;
		files = r.Files || files;
		endNs = r.EndNs;
		summary = r.Replay || summary;
		for (const row of r.Rows || []) rows[row.I] = row;
		loaded = Math.max(loaded, (r.From || 0) + (r.Rows || []).length);
		if (loaded < total && (r.Rows || []).length > 0) {
			$('insnSummary').textContent = `Loading the instructions… ${loaded} of ${total}`;
			loading = true;
			vscode.postMessage({ type: 'insns', from: loaded, count: PAGE, scope: scope() });
			return;
		}
		showSummary();
		build();
		vscode.postMessage({ type: 'sources', files });
		if (pendingRow !== undefined && total > 0) {
			select(Math.min(pendingRow, total - 1), true);
			pendingRow = undefined;
		} else if (sel < 0 && total > 0) select(replayAt ?? total - 1, true);
	}

	function showSummary() {
		const s = summary || {};
		const pct = (x) => (x === null || x === undefined ? '?' : Math.round(100 * x) + ' %');
		let text = `${total} instructions`;
		if (s.History) {
			const h = s.History;
			text += ` in ${h.Actions} action${h.Actions === 1 ? '' : 's'} of the reverse history (${h.Recorded} recorded` +
				(h.LostStart ? `, ${h.LostStart} without their start` : '') + ')';
			$('insnSummary').innerHTML = text;
			return;
		}
		const old = result && result.RanSince;
		if (old) {
			text = `<span class="warn">This trace ${esc(window.traceRanSinceText(old))}; it does not lead to the current halt.</span> ` + text;
		}
		if (!window.aurixReverse) $('insnReplay').disabled = !!old;
		if (s.Anchored) {
			text += ` · registers rebuilt from the halt: ${pct(s.KnownNearHalt)} known at the halt, ${pct(s.KnownAtStart)} at the start`;
			if (!s.Memory) text += ' · <span class="warn">memory at the halt not read (the core ran since)</span>';
		} else {
			text += ' · <span class="warn">the trace does not end at the core\'s halt: only constants are known</span>';
		}
		if (s.Entries) text += ` · ${s.Entries} interrupt entr${s.Entries === 1 ? 'y' : 'ies'}`;
		if (s.Unmodelled) {
			const top = (s.UnmodelledTop || []).map((x) => `${x[0]} ${x[1]}×`).join(', ');
			text += ` · <span title="Their results are unknown: ${esc(top)}">${s.Unmodelled} instructions not modelled</span>`;
		}
		$('insnSummary').innerHTML = text;
	}

	// ---- display list -------------------------------------------------------------------------

	function isCall(r) {
		return r && r.K === 'i' && /^(f?call[ai]?)$/.test(r.M);
	}
	function isRet(r) {
		return r && r.K === 'i' && /^(ret|rfe|fret)$/.test(r.M);
	}

	function build() {
		display = [];
		rowPos = new Map();
		let fn;
		let loc;
		let act;
		for (let i = 0; i < total; i++) {
			const r = rows[i];
			if (!r) continue;
			if (r.Act !== undefined && r.Act !== act) {
				act = r.Act;
				display.push({ k: 'act', i, d: 0, act: r.Act, what: r.ActKind });
				fn = undefined;
				loc = undefined;
			}
			if (r.K === 'm') {
				display.push({ k: 'mark', i, d: 0 });
				rowPos.set(i, display.length - 1);
				continue;
			}
			if (r.K === 'g') {
				display.push({ k: 'gap', i, d: 0 });
				rowPos.set(i, display.length - 1);
				fn = undefined;
				loc = undefined;
				continue;
			}
			if (r.K === 'e') {
				display.push({ k: 'entry', i, d: r.D || 0 });
				rowPos.set(i, display.length - 1);
				fn = undefined;
				loc = undefined;
				continue;
			}
			if (callsOnly && !isCall(r) && !isRet(r)) continue;
			if (r.F !== fn) {
				fn = r.F;
				loc = undefined;
				display.push({ k: 'fn', i, d: r.D || 0, f: r.F });
			}
			const l = `${r.Fl}:${r.L}`;
			if (showSrc && !callsOnly && r.L && l !== loc) {
				display.push({ k: 'src', i, d: r.D || 0, file: files[r.Fl], line: r.L });
			}
			loc = l;
			display.push({ k: 'row', i, d: r.D || 0 });
			rowPos.set(i, display.length - 1);
		}
		if (result && result.Halt) display.push({ k: 'halt', d: 0 });
		$('insnSpacer').style.height = display.length * ROW_H + 'px';
		wide = 0;
		$('insnRows').style.width = '';
		const list = $('insnList');
		list.scrollTop = Math.min(list.scrollTop, Math.max(0, display.length * ROW_H - list.clientHeight));
		draw();
	}

	function effects(r) {
		if (!showEff) return '';
		const parts = [];
		if (r.B === true) parts.push('<span class="br-t">taken</span>');
		if (r.B === false) parts.push('<span class="br-n">not taken</span>');
		for (const w of r.W || []) parts.push(`${w[0]} ← ${val(w[1], w[2], w[3])}`);
		for (const x of r.X || []) {
			const a = x.A !== null && x.A !== undefined ? hex(x.A) : '[?]';
			const v = x.V ? x.V.map((w) => hex(w)).join('_') : '?';
			parts.push(x.S ? `<span class="mem">[${a}] ← ${v}</span>` : `<span class="mem">load [${a}]</span>`);
		}
		if (isCall(r)) parts.push('<span class="mem">context saved</span>');
		if (isRet(r)) parts.push('<span class="mem">context restored</span>');
		return parts.join('   ');
	}

	function timeCell(i) {
		const r = rows[i];
		if (!r || r.T === null || r.T === undefined || endNs === undefined || endNs === null) return '';
		const prev = i > 0 ? rows[i - 1] : undefined;
		if (prev && prev.T === r.T) return '';
		return fmtNs(r.T - endNs);
	}

	function itemHtml(it, n) {
		const top = n * ROW_H;
		// Placed by draw(): the webview's CSP drops inline style attributes.
		const pad = `data-top="${top}" data-depth="${Math.min(it.d, 30)}"`;
		switch (it.k) {
			case 'fn':
				return `<div class="it fn" ${pad}>${esc(funcs[it.f] ?? '(no symbol)')}</div>`;
			case 'src': {
				const lines = sources[it.file];
				const text = lines ? (lines[it.line - 1] ?? '').trim() : '';
				return `<div class="it src" ${pad}><a href="#" class="src" data-file="${esc(it.file)}" data-line="${it.line}">${esc(baseName(it.file))}:${it.line}</a> <span class="code">${esc(text)}</span></div>`;
			}
			case 'gap': {
				const note = rows[it.i] && rows[it.i].Note;
				return `<div class="it gap" ${pad} data-i="${it.i}">⋯ ${note ? esc(note) : 'gap: code ran that the trace does not show (registers unknown after it)'}</div>`;
			}
			case 'act':
				return `<div class="it act" ${pad}>▸ action ${it.act}: ${esc(it.what || '')}</div>`;
			case 'mark':
				return `<div class="it mark${it.i === sel ? ' sel' : ''}" ${pad} data-i="${it.i}">◆ ${esc(rows[it.i].Note || '')}</div>`;
			case 'entry': {
				const r = rows[it.i];
				return `<div class="it entry${it.i === sel ? ' sel' : ''}" ${pad} data-i="${it.i}">↘ interrupt or trap entry at ${hex(r.A)}${r.Resume ? `, returns to ${hex(r.Resume)}` : ''}</div>`;
			}
			case 'halt': {
				const h = result.Halt || {};
				const then = result.RanSince ? ` (${esc(window.traceRanSinceText(result.RanSince).replace(/^ends at /, ''))})` : '';
				return `<div class="it halt" ${pad}>● the core halted here${then}: ${esc(h.Function || '')} ${h.File ? esc(baseName(h.File)) + ':' + (h.Line || '?') : ''} (${hex(h.Addr)}), not executed</div>`;
			}
			default: {
				const r = rows[it.i];
				const cls = `it row${it.i === sel ? ' sel' : ''}${replayAt !== undefined && it.i === replayAt + 1 ? ' pc' : ''}${isCall(r) ? ' call' : ''}${isRet(r) ? ' ret' : ''}`;
				return `<div class="${cls}" ${pad} data-i="${it.i}"><span class="n">${it.i + 1}</span><span class="t">${timeCell(it.i)}</span>` +
					`<span class="a">${(r.A >>> 0).toString(16)}</span><span class="i"><span class="m">${esc(r.M)}</span>${esc(r.O)}</span>` +
					`<span class="e">${effects(r)}</span></div>`;
			}
		}
	}

	function draw() {
		const list = $('insnList');
		const first = Math.max(0, Math.floor(list.scrollTop / ROW_H) - 10);
		const last = Math.min(display.length, Math.ceil((list.scrollTop + list.clientHeight) / ROW_H) + 10);
		let html = '';
		for (let n = first; n < last; n++) html += itemHtml(display[n], n);
		const box = $('insnRows');
		box.innerHTML = html;
		for (const el of box.children) {
			el.style.top = el.dataset.top + 'px';
			el.style.setProperty('--d', el.dataset.depth);
		}
		let w = wide;
		for (const el of box.children) w = Math.max(w, el.scrollWidth);
		if (w > wide) {
			wide = w;
			box.style.width = w + 'px';
		}
	}

	function scrollTo(i) {
		let n = rowPos.get(i);
		if (n === undefined) {
			// Not shown (Calls only): the nearest shown row before it.
			n = 0;
			for (const [row, pos] of rowPos) if (row <= i && pos > n) n = pos;
		}
		const list = $('insnList');
		const y = n * ROW_H;
		if (y < list.scrollTop + ROW_H * 2) list.scrollTop = Math.max(0, y - ROW_H * 4);
		else if (y > list.scrollTop + list.clientHeight - ROW_H * 3) list.scrollTop = y - list.clientHeight + ROW_H * 6;
		draw();
	}

	function select(i, scroll) {
		if (i < 0 || i >= total) return;
		sel = i;
		if (scroll) scrollTo(i);
		draw();
		clearTimeout(regsTimer);
		regsTimer = setTimeout(() => {
			if (regsFor !== sel) vscode.postMessage({ type: 'regs', at: sel, scope: scope() });
		}, 60);
	}

	function move(delta) {
		const selectable = (x) => x && (x.k === 'row' || x.k === 'entry' || x.k === 'mark');
		let n = rowPos.get(sel) ?? 0;
		n = Math.max(0, Math.min(display.length - 1, n + delta));
		const step = delta < 0 ? -1 : 1;
		let k = n;
		while (k >= 0 && k < display.length && !selectable(display[k])) k += step;
		if (!selectable(display[k])) {
			k = n;
			while (k >= 0 && k < display.length && !selectable(display[k])) k -= step;
		}
		if (selectable(display[k])) select(display[k].i, true);
	}

	// ---- registers ----------------------------------------------------------------------------

	function renderRegs() {
		if (!regs) return;
		const by = new Map((regs.Regs || []).map((r) => [r.N, r]));
		const cell = (n) => {
			const r = by.get(n);
			if (!r) return '<div></div>';
			return `<div class="reg${r.W ? ' w' : ''}${n === pick ? ' pick' : ''}" data-r="${n}"><span class="k">${n.toUpperCase()}</span>${val(r.V, r.M, r.K)}</div>`;
		};
		const pair = (a, b) => cell(a) + cell(b);
		const d = [];
		const a = [];
		for (let k = 0; k < 8; k++) {
			d.push(pair('d' + k, 'd' + (k + 8)));
			a.push(pair('a' + k, 'a' + (k + 8)));
		}
		const p = by.get(pick);
		const r = rows[regs.At];
		const what = r && r.K === 'i' ? `${(r.A >>> 0).toString(16)}  ${esc(r.M)} ${esc(r.O)}` : r && r.K === 'e' ? 'interrupt entry' : r && r.Note ? esc(r.Note) : 'gap';
		const frames = (regs.Frames || []).map((f, k) => {
			const where = f.File ? `<a href="#" class="src" data-file="${esc(f.File)}" data-line="${f.Line || 0}">${esc(baseName(f.File))}:${f.Line || '?'}</a>` : hex(f.Pc);
			return `<div class="frame${k === 0 ? ' top' : ''}">${esc(f.Function || '?')} <span class="hint">${where}</span></div>`;
		}).join('');
		$('insnRegs').innerHTML =
			`<h3>After #${regs.At + 1}</h3><div class="at">${what}</div>` +
			`<h3>Data registers</h3><div class="grid">${d.join('')}</div>` +
			`<h3>Address registers</h3><div class="grid">${a.join('')}</div>` +
			`<h3>System</h3><div class="grid"><div class="reg"><span class="k">PC</span>${val(regs.Pc, regs.Pc === null || regs.Pc === undefined ? undefined : 'exact')}</div>${cell('psw')}${cell('pcxi')}${cell('fcx')}${cell('icr')}</div>` +
			(p ? `<div class="why"><b>${esc(p.N.toUpperCase())} = ${p.V === null || p.V === undefined ? 'unknown' : (p.M === 'mem' ? '≈' : '') + hex(p.V)}</b><br>${esc(p.Why || '')}</div>` : '') +
			`<h3>Call stack at this point</h3><div class="stack">${frames}</div>`;
	}

	// ---- wiring -------------------------------------------------------------------------------

	// The tab shown: timeline, list (the Flow pane, traceTimeline.js), insns or history.
	let current = ((vscode.getState && vscode.getState()) || {}).flowView || 'timeline';
	function setTab(tab) {
		if (tab === 'flow') tab = window.traceFlowView ? window.traceFlowView() : 'timeline';
		current = tab;
		active = tab === 'insns';
		document.querySelectorAll('#tabs .tab').forEach((b) => b.classList.toggle('on', b.dataset.tab === tab));
		$('insns').hidden = !active;
		$('flowPane').hidden = tab !== 'timeline' && tab !== 'list';
		$('history').hidden = tab !== 'history';
		$('insnToolbar').hidden = !active;
		window.dispatchEvent(new CustomEvent('trace-tab', { detail: tab }));
		if (window.traceFitHeader) window.traceFitHeader();
		if (active) {
			if (result && result.Kind === 'program' && !total && !loading) load();
			draw();
		}
	}

	document.querySelectorAll('#tabs .tab').forEach((b) => b.addEventListener('click', () => setTab(b.dataset.tab)));
	window.traceTab = () => current;

	// The Timeline (traceTimeline.js): show a row of the last recording here.
	let pendingRow;
	window.traceGotoRow = (row) => {
		if (historyMode()) {
			// Rows of the whole history are numbered differently: show the last action's rows.
			wholeHistory = false;
			$('insnHistory').classList.remove('on');
			reset(result);
		}
		pendingRow = row;
		setTab('insns');
		if (total && loaded >= total && !loading) {
			select(Math.min(row, total - 1), true);
			pendingRow = undefined;
		}
	};
	$('insnList').addEventListener('scroll', draw);
	$('insnList').addEventListener('click', (e) => {
		if (e.target.closest('a.src')) return;
		const it = e.target.closest('[data-i]');
		if (it) select(Number(it.dataset.i), false);
	});
	$('insnList').addEventListener('dblclick', (e) => {
		const it = e.target.closest('[data-i]');
		const r = it && rows[Number(it.dataset.i)];
		if (r && r.Fl !== undefined && r.Fl !== null) vscode.postMessage({ type: 'open', file: files[r.Fl], line: r.L });
	});
	$('insnList').addEventListener('keydown', (e) => {
		const page = Math.max(1, Math.floor($('insnList').clientHeight / ROW_H) - 2);
		const keys = { ArrowDown: 1, ArrowUp: -1, PageDown: page, PageUp: -page };
		if (keys[e.key]) {
			move(keys[e.key]);
			e.preventDefault();
		} else if (e.key === 'Home') {
			select(0, true);
			e.preventDefault();
		} else if (e.key === 'End') {
			select(total - 1, true);
			e.preventDefault();
		} else if (e.key === 'Enter') {
			const r = rows[sel];
			if (r && r.Fl !== undefined && r.Fl !== null) vscode.postMessage({ type: 'open', file: files[r.Fl], line: r.L });
		}
	});
	$('insnRegs').addEventListener('click', (e) => {
		const c = e.target.closest('.reg[data-r]');
		if (c) {
			pick = c.dataset.r;
			renderRegs();
		}
	});
	const toggle = (id, f) => $(id).addEventListener('click', () => {
		f();
		$(id).classList.toggle('on');
		build();
		if (sel >= 0) scrollTo(sel);
	});
	toggle('insnSrc', () => (showSrc = !showSrc));
	toggle('insnEff', () => (showEff = !showEff));
	toggle('insnCalls', () => (callsOnly = !callsOnly));
	$('insnHistory').addEventListener('click', () => {
		wholeHistory = !wholeHistory;
		$('insnHistory').classList.toggle('on', wholeHistory);
		reset(result);
	});
	// Reverse debugging switched on or off, or its history changed (also by steps, which record
	// nothing): the whole-history view is read again.
	window.addEventListener('reverse-changed', () => {
		$('insnHistory').hidden = !window.aurixReverse;
		if (historyMode() || total) reset(result);
	});
	$('insnStart').addEventListener('click', () => select(0, true));
	$('insnEnd').addEventListener('click', () => select(total - 1, true));
	$('insnFind').addEventListener('keydown', (e) => {
		if (e.key === 'Enter') {
			vscode.postMessage({ type: 'find', query: $('insnFind').value, from: sel >= 0 ? sel : undefined, back: e.shiftKey, scope: scope() });
			e.preventDefault();
		}
	});
	$('insnReplay').addEventListener('click', () => {
		if (sel < 0) return;
		if (window.aurixReverse) {
			// Reverse debugging: restore the target to the point before the selected row (whole
			// history: the row's action and its row there; else a row of the last action).
			const r = rows[sel];
			if (r && r.Act !== undefined) vscode.postMessage({ type: 'reverseGoto', index: r.Act, row: r.R ?? 0 });
			else vscode.postMessage({ type: 'reverseGoto', row: sel });
		} else {
			vscode.postMessage({ type: 'replayAt', at: sel });
		}
	});
	window.addEventListener('message', (e) => {
		if (e.data && e.data.type === 'showTab') setTab(e.data.tab);
	});
	$('insnReplayOff').addEventListener('click', () => vscode.postMessage({ type: 'replayOff' }));

	window.addEventListener('trace-ran-since', () => {
		if (!result) return;
		showSummary();
		draw();
	});

	window.addEventListener('trace-result', (e) => {
		reset(e.detail);
		// A data recording has only its list of writes.
		setTab(e.detail && e.detail.Kind !== 'program' ? 'list' : current);
	});

	window.addEventListener('message', (e) => {
		const m = e.data;
		switch (m.type) {
			case 'insnsPage':
				onPage(m);
				break;
			case 'regs':
				if (m.error) {
					$('insnRegs').innerHTML = `<div class="bad">${esc(m.error)}</div>`;
				} else if (m.result && m.result.At === sel) {
					regs = m.result;
					regsFor = regs.At;
					renderRegs();
				}
				break;
			case 'found':
				if (m.result && m.result.At !== null && m.result.At !== undefined) {
					select(m.result.At, true);
				} else {
					$('insnFind').classList.add('miss');
					setTimeout(() => $('insnFind').classList.remove('miss'), 800);
				}
				break;
			case 'sources':
				sources = m.sources || {};
				draw();
				break;
			case 'replay':
				replayAt = m.active ? m.at : undefined;
				$('insnReplayOff').hidden = !m.active;
				if (m.active && typeof m.at === 'number') {
					if (!active) setTab('insns');
					select(m.at, true);
				} else {
					draw();
				}
				break;
		}
	});
})();
