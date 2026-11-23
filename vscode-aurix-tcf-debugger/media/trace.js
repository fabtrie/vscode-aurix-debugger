// Trace panel webview: the configuration form, the check result, the state and the decoded trace.
(function () {
	const vscode = acquireVsCodeApi();
	// The Instructions view (traceInsns.js) posts through the same API (it can be acquired once).
	window.traceVscode = vscode;
	const $ = (id) => document.getElementById(id);
	const MAX_ROWS_PER_BLOCK = 400;

	let config = {};
	let armed = false;
	let result;
	let applying = false;
	// The form is open (the header shows the configuration as a summary otherwise).
	let editing = true;
	// The configuration changed since it was armed or since the shown recording.
	let dirty = false;
	// The check's verdict, for the summary's tooltip.
	let checkText = '';
	const num = (n) => Number(n).toLocaleString('en-US').replace(/,/g, '\u2009');

	function esc(s) {
		return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
	}

	function fmtNs(ns) {
		if (ns === null || ns === undefined) {
			return '';
		}
		const a = Math.abs(ns);
		if (a >= 1e6) return (ns / 1e6).toFixed(3) + ' ms';
		if (a >= 1e3) return (ns / 1e3).toFixed(2) + ' µs';
		return Math.round(ns) + ' ns';
	}

	function hex(n) {
		return n === null || n === undefined ? '' : '0x' + Number(n).toString(16).toUpperCase().padStart(8, '0');
	}

	/** "0xLO..0xHI" → address range; anything else names a function. */
	function parseRange(text) {
		const t = (text || '').trim();
		if (!t) return undefined;
		const m = /^(0x[0-9a-f]+|\d+)\s*\.\.\s*(0x[0-9a-f]+|\d+)$/i.exec(t);
		if (m) {
			const lo = Number(m[1]);
			const hi = Number(m[2]);
			return { Start: m[1], Len: Math.max(0, hi - lo) };
		}
		return { Function: t };
	}

	function rangeText(r) {
		if (!r) return '';
		if (r.Function) return r.Function;
		const lo = Number(r.Start);
		return hex(lo) + '..' + hex(lo + Number(r.Len || 0));
	}

	function radio(name) {
		const el = document.querySelector(`input[name="${name}"]:checked`);
		return el ? el.value : undefined;
	}

	function setRadio(name, value) {
		const el = document.querySelector(`input[name="${name}"][value="${value}"]`);
		if (el) el.checked = true;
	}

	function readForm() {
		const kind = radio('kind') || 'all';
		const c = { Core: Number($('core').value), Timestamps: radio('ts') || 'tsu' };
		if (kind === 'data') {
			c.Kind = 'data';
			const r = parseRange($('dataRange').value);
			c.Ranges = r ? [r] : [];
		} else {
			c.Kind = 'program';
			const r = kind === 'inside' ? parseRange($('range').value) : undefined;
			c.Ranges = r ? [r] : [];
			if ($('context').checked) c.Context = true;
		}
		const stop = $('stop').value;
		c.Stop = stop === 'function' ? { Function: $('stopAt').value.trim() } : stop;
		return c;
	}

	function applyForm(c) {
		applying = true;
		$('core').value = String(c.Core ?? 0);
		const r = (c.Ranges || [])[0];
		if (c.Kind === 'data') {
			setRadio('kind', 'data');
			$('dataRange').value = rangeText(r);
		} else if (r) {
			setRadio('kind', 'inside');
			$('range').value = rangeText(r);
		} else {
			setRadio('kind', 'all');
		}
		if (c.Stop && typeof c.Stop === 'object') {
			$('stop').value = 'function';
			$('stopAt').value = c.Stop.Function || '';
		} else {
			$('stop').value = c.Stop || 'halt';
		}
		setRadio('ts', c.Timestamps || 'tsu');
		$('context').checked = !!c.Context;
		applying = false;
		updateFormState();
		renderSummary();
	}

	/** The header's short summary of the configuration; all of it in the tooltip. */
	function renderSummary() {
		const c = config || {};
		const sel = $('saved').value;
		$('cfgName').textContent = sel || 'unsaved';
		const chip = $('cfgChip');
		if (window.aurixReverse) {
			$('cfgSum').textContent = 'reverse debugging';
			chip.title = 'The trace is used by reverse debugging';
			return;
		}
		const r = (c.Ranges || [])[0];
		const short = [`CPU${c.Core ?? 0}`];
		if (c.Kind === 'data') short.push(`writes into ${rangeText(r) || '?'}`);
		else if (r) short.push(`inside ${rangeText(r)}`);
		else short.push('All code');
		if (c.Kind !== 'data' && c.Context) short.push('task / ISR');
		const stop = c.Stop;
		const stopText = stop && typeof stop === 'object' ? `stop at ${stop.Function || '?'}` : stop === 'full' ? 'first 8 KB' : stop === 'manual' ? 'stop by hand' : 'stop on halt';
		const tsText = c.Timestamps === 'ticks' ? 'tick time stamps' : 'time stamps with messages';
		const sep = '<span class="sep">·</span>';
		$('cfgSum').innerHTML = (sel ? `<span class="nm">${esc(sel)}</span>${sep}` : '') + short.map(esc).join(sep) +
			(dirty ? ' <span class="dirty">●</span>' : '');
		chip.title = `${sel || 'Unsaved configuration'}: ${[...short, stopText, tsText].join(' · ')}` +
			(checkText ? `\n${checkText}` : '') + (dirty ? '\n● changed since it was armed' : '') + '\nClick to change it';
	}

	/** The configuration popover. */
	function setEditing(v) {
		editing = !!v && !window.aurixReverse;
		$('cfgPop').hidden = !editing;
		$('cfgChip').classList.toggle('open', editing);
		$('cfgChev').textContent = editing ? '▴' : '▾';
		if (editing) document.querySelectorAll('.menu').forEach((x) => (x.hidden = true));
	}
	/** Opens the configuration, pointing at one of its fields (traceTimeline.js: the ⓘ). */
	window.traceEditConfig = (focus) => {
		setEditing(true);
		const el = focus && $(focus);
		if (el) {
			el.classList.add('flash');
			setTimeout(() => el.classList.remove('flash'), 1600);
		}
	};

	/** Two header rows when one is not enough: the tab's tools then take the second row. */
	function fitHeader() {
		const tb = $('toolbar');
		const tools = $('tools');
		tools.hidden = !tools.querySelector('.toolset:not([hidden])');
		tb.classList.remove('two');
		const first = $('tabs').hidden ? $('title') : $('tabs');
		const top = first.getBoundingClientRect().top;
		if ([$('rec'), tools].some((el) => !el.hidden && el.getBoundingClientRect().top > top + 6)) tb.classList.add('two');
		// Where the tab's content starts: the timeline and the instruction list fill the rest.
		let h = tb.getBoundingClientRect().bottom;
		for (const el of [$('reverseBanner'), $('message')]) {
			if (el.offsetParent !== null) h = Math.max(h, el.getBoundingClientRect().bottom);
		}
		document.documentElement.style.setProperty('--hdr', Math.ceil(h + window.scrollY) + 'px');
	}
	window.traceFitHeader = fitHeader;
	window.addEventListener('resize', fitHeader);

	function updateFormState() {
		const kind = radio('kind') || 'all';
		$('range').disabled = kind !== 'inside';
		$('dataRange').disabled = kind !== 'data';
		const stop = $('stop').value;
		$('stopAt').hidden = stop !== 'function';
		$('stopHint').textContent =
			stop === 'halt' ? 'The debugger stops it when the core halts: the trace runs up to the halt, and Step Back replays it.' :
			stop === 'full' ? 'A single buffer: the first 8 KB of the run.' :
			stop === 'manual' ? 'A ring: the last 8 KB before "Stop and read".' :
			'The trace ends when the function is entered; the ring keeps the 8 KB before.';
		$('keep').textContent = '';
		$('rangeInfo').textContent = kind === 'inside' ? 'Only code in this range is recorded.' : '';
		$('rangeField').hidden = kind !== 'inside';
		$('dataField').hidden = kind !== 'data';
		$('contextLabel').hidden = kind === 'data';
		$('context').disabled = kind === 'data';
		if (window.aurixReverse) {
			$('range').disabled = true;
			$('dataRange').disabled = true;
			$('context').disabled = true;
		}
		// One record button: Arm, or Stop and read while armed (Re-arm is in the configuration).
		$('armPop').textContent = armed ? 'Re-arm' : 'Arm';
		$('arm').hidden = armed;
		$('stopBtn').hidden = !armed;
		fitHeader();
	}

	let timer;
	function changed() {
		if (applying) return;
		updateFormState();
		config = readForm();
		$('saved').value = '';
		dirty = armed || !!result;
		renderSummary();
		clearTimeout(timer);
		timer = setTimeout(() => vscode.postMessage({ type: 'config', config }), 250);
	}

	function setState(text, cls, title) {
		const s = $('state');
		s.textContent = text;
		s.className = 'state ' + cls;
		s.title = title || '';
	}

	function message(text, cls) {
		const m = $('message');
		// With a recording shown, notes go into the state's tooltip: the header keeps its height.
		if (text && (cls || 'info') === 'info' && result) {
			$('state').title = text;
			text = '';
		}
		m.textContent = text || '';
		m.className = text ? cls || 'info' : '';
		fitHeader();
	}

	function showCheck(m) {
		const el = $('resources');
		const budget = $('budget');
		if (!m.ok) {
			el.innerHTML = `<span class="bad">Cannot be armed: ${esc(m.error)}</span>`;
			budget.innerHTML = '<span class="bad">cannot be armed</span>';
			budget.title = m.error || '';
			$('arm').disabled = true;
			$('armPop').disabled = true;
			$('cfgChip').classList.add('bad');
			checkText = `Cannot be armed: ${m.error || ''}`;
			renderSummary();
			// Show why, in the form.
			setEditing(true);
			return;
		}
		$('arm').disabled = !!window.aurixReverse;
		$('armPop').disabled = !!window.aurixReverse;
		$('cfgChip').classList.remove('bad');
		const c = m.check || {};
		const r = c.Resources || {};
		const events = typeof r.Events === 'number' ? r.Events : (r.Events || []).length;
		const full =
			`Uses ${r.PtuComparators ?? 0} of 2 program comparators, ${r.DtuComparators ?? 0} of 2 data comparators, ` +
			`${events} of 16 events · buffer ${c.Ring ? 'ring' : 'linear'} 8 KB · covers ${c.Window || ''}`;
		el.textContent = full;
		const win = String(c.Window || '').replace(/^about /, '≈ ').replace(/ of execution$/, '');
		budget.textContent = `${win} · ${r.PtuComparators ?? 0}/2 program, ${r.DtuComparators ?? 0}/2 data comparators · ${events}/16 events`;
		budget.title = full;
		checkText = budget.textContent;
		$('arm').title = full;
		renderSummary();
	}

	/** Why the recording does not end where the core stands now (`RanSince`), as a phrase. */
	function ranSinceText(why) {
		const what = why === 'reset' ? 'the device was reset' : why === 'step' ? 'the core stepped' : 'the core ran';
		return `ends at an earlier halt: ${what} since`;
	}
	window.traceRanSinceText = ranSinceText;

	function showStatus(st, error) {
		if (error) {
			setState('no session', 'idle');
			armed = false;
			updateFormState();
			return;
		}
		armed = !!(st && st.Armed && st.Owner === 'snapshot');
		if (st && st.Armed && st.Owner === 'daq') {
			setState('used by Live Watch', 'busy');
			message('The trace is in use by a Live Watch or OS Profile in trace mode. Stop it there to record here.', 'warn');
		} else if (armed) {
			const fill = st.Fill !== undefined ? ` · ${Math.round(st.Fill * 100)} % full` : '';
			setState(st.Done ? 'done' : 'armed' + fill, st.Done ? 'done' : 'armed', fill ? 'How much of the 8 KB trace buffer holds data. A recording that stops on a halt, at a function or by hand keeps running at 100 %, overwriting the oldest data, and keeps the last 8 KB.' : '');
		} else if (st && st.LostByReset) {
			setState('lost by reset', 'warn');
		} else {
			setState('idle', 'idle');
		}
		$('stopBtn').disabled = !armed || !!window.aurixReverse;
		updateFormState();
		// The shown recording: the core ran, stepped or was reset since it ended.
		if (result && result.Kind === 'program' && st && st.Last && st.Last.stopped_ms === result.StoppedMs) {
			const rs = st.Last.ran_since || null;
			if (rs !== (result.RanSince || null)) {
				result.RanSince = rs;
				renderProgram(result);
				window.dispatchEvent(new CustomEvent('trace-ran-since', { detail: rs }));
			}
		}
	}

	// ---- result ------------------------------------------------------------------------------

	function openSrc(file, line) {
		if (file) vscode.postMessage({ type: 'open', file, line });
	}

	function baseName(p) {
		return String(p || '').split(/[\\/]/).pop();
	}

	/** Program flow: consecutive instructions of one function form a block, gaps break blocks. */
	function blocks(r) {
		const out = [];
		let cur;
		for (const s of r.Steps || []) {
			if (!Array.isArray(s)) {
				cur = undefined;
				out.push({ gap: s.Gap, t: s.T });
				continue;
			}
			const fi = s[2];
			if (!cur || cur.fi !== fi) {
				cur = { fi, rows: [] };
				out.push(cur);
			}
			cur.rows.push(s);
		}
		return out;
	}

	function gapText(g) {
		return g === 'trace_off' ? 'outside the range (trace off)' :
			g === 'no_code' ? 'no code at the address' :
			g === 'inconsistent' ? 'trace and code disagree' :
			g === 'lost' ? 'messages lost' :
			g === 'untraced' ? 'the end before the halt is not in the trace (data lost)' : String(g);
	}

	function renderProgram(r) {
		const st = r.Stats || {};
		const fn = r.Functions || [];
		const files = r.Files || [];
		const bl = blocks(r);
		const counts = new Map();
		for (const b of bl) {
			if (b.rows) counts.set(b.fi, (counts.get(b.fi) || 0) + b.rows.length);
		}
		const total = [...counts.values()].reduce((a, b) => a + b, 0) || 1;
		const top = [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
		$('coverage').innerHTML =
			`<div class="summary">${st.Instructions ?? 0} instructions · ${st.Gaps ?? 0} gaps` +
			(st.Lost ? ` · <span class="bad">${st.Lost} messages lost</span>` : '') +
			(r.Truncated ? ' · newest part shown' : '') +
			(r.Done ? '' : ' · <span class="warn">stopped before the end condition</span>') +
			`</div><div class="bars">` +
			top.map(([fi, n]) => {
				const pct = (100 * n) / total;
				return `<div class="bar" title="${esc(fn[fi] ?? '?')}: ${n} instructions"><span class="name">${esc(fn[fi] ?? '?')}</span>` +
					`<span class="track"><span class="fill" data-pct="${pct.toFixed(1)}"></span></span><span class="pct">${pct.toFixed(0)} %</span></div>`;
			}).join('') + `</div>`;
		// Widths through the DOM: the webview's CSP drops inline style attributes.
		document.querySelectorAll('#coverage .fill[data-pct]').forEach((el) => (el.style.width = el.dataset.pct + '%'));

		const last = [...(r.Steps || [])].reverse().find((s) => Array.isArray(s));
		const halt = r.Halt;
		let html = '';
		if (halt && halt.File) {
			// Where the core stands: the flow above ends with the instruction before it.
			const where = `${esc(halt.Function || hex(halt.Addr))}, ${esc(baseName(halt.File))}:${halt.Line || '?'}`;
			html += `<div class="haltrow"><a href="#" class="src" data-file="${esc(halt.File)}" data-line="${halt.Line || 0}">Go to the halt point (${where})</a>` +
				(r.ReachesHalt ? ' <span class="hint">· traced up to it</span>' : ' <span class="warn">· the end before it is not in the trace</span>') +
				(r.RanSince ? ` <span class="warn">· this trace ${ranSinceText(r.RanSince)}</span>` : '') + '</div>';
		} else if (last && files[last[3]]) {
			html += `<div class="haltrow"><a href="#" class="src" data-file="${esc(files[last[3]])}" data-line="${last[4] || 0}">Go to the last instruction (${esc(baseName(files[last[3]]))}:${last[4] || '?'})</a></div>`;
		}
		html += '<div class="flow">';
		bl.forEach((b, i) => {
			if (!b.rows) {
				html += `<div class="gap">⋯ gap: ${esc(gapText(b.gap))}${b.t !== null && b.t !== undefined ? ' at ' + fmtNs(b.t) : ''}</div>`;
				return;
			}
			const first = b.rows[0];
			const lastRow = b.rows[b.rows.length - 1];
			const lines = b.rows.map((s) => s[4]).filter((l) => l);
			const lo = lines.length ? Math.min(...lines) : undefined;
			const hi = lines.length ? Math.max(...lines) : undefined;
			const file = files[first[3]];
			const t0 = first[1];
			const t1 = lastRow[1];
			html += `<details class="block"${bl.length <= 3 ? ' open' : ''}><summary>` +
				`<span class="fn">${esc(fn[b.fi] ?? hex(first[0]))}</span>` +
				`<span class="where">${file ? `<a href="#" class="src" data-file="${esc(file)}" data-line="${lo || 0}">${esc(baseName(file))}${lo ? ':' + lo + (hi !== lo ? '–' + hi : '') : ''}</a>` : ''}</span>` +
				`<span class="n">${b.rows.length} insn</span>` +
				`<span class="t">${fmtNs(t0)}${t1 !== t0 && t1 !== null && t1 !== undefined ? ' … ' + fmtNs(t1) : ''}</span>` +
				`</summary><table class="rows" data-block="${i}"><tbody>`;
			const rows = b.rows.length > MAX_ROWS_PER_BLOCK ? b.rows.slice(-MAX_ROWS_PER_BLOCK) : b.rows;
			if (rows !== b.rows) {
				html += `<tr><td colspan="3" class="hint">${b.rows.length - rows.length} earlier instructions not listed</td></tr>`;
			}
			let prevLine;
			for (const s of rows) {
				const f = files[s[3]];
				const lineCell = s[4] && s[4] !== prevLine && f ? `<a href="#" class="src" data-file="${esc(f)}" data-line="${s[4]}">${s[4]}</a>` : '';
				prevLine = s[4];
				html += `<tr><td class="addr">${hex(s[0])}</td><td class="line">${lineCell}</td><td class="t">${fmtNs(s[1])}</td></tr>`;
			}
			html += '</tbody></table></details>';
		});
		html += '</div>';
		$('result').innerHTML = html;
	}

	function renderData(r) {
		const st = r.Stats || {};
		$('coverage').innerHTML = `<div class="summary">${st.Events ?? 0} writes` +
			(st.Lost ? ` · <span class="bad">${st.Lost} messages lost</span>` : '') +
			(r.Done ? '' : ' · <span class="warn">stopped before the end condition</span>') + '</div>';
		let html = '<table class="data"><thead><tr><th>Time</th><th>Variable</th><th>Address</th><th>Value</th><th>Size</th></tr></thead><tbody>';
		for (const e of r.Events || []) {
			if (e.Lost !== undefined) {
				html += `<tr class="lost"><td>${fmtNs(e.T)}</td><td colspan="4">${e.Lost} messages lost</td></tr>`;
				continue;
			}
			html += `<tr><td>${fmtNs(e.T)}</td><td>${esc(e.Symbol || '')}</td><td class="addr">${hex(e.Addr)}</td>` +
				`<td class="addr">${e.Value !== undefined && e.Value !== null ? '0x' + Number(e.Value).toString(16).toUpperCase() : ''}</td><td>${e.Size ?? ''}</td></tr>`;
		}
		html += '</tbody></table>';
		$('result').innerHTML = html;
	}

	function showResult(r) {
		result = r;
		if (r) {
			// A recording: the configuration folds into the header.
			dirty = false;
			setEditing(false);
			renderSummary();
		}
		$('export').disabled = !r;
		message('');
		if (!r) {
			$('coverage').innerHTML = '';
			$('result').innerHTML = '';
		} else if (r.Kind === 'program') {
			renderProgram(r);
		} else {
			renderData(r);
		}
		// The tabs: Timeline, List and Instructions for a program recording, the writes for data.
		const program = !!(r && r.Kind === 'program');
		const s = (r && r.Stats) || {};
		$('tabs').hidden = !r && !window.aurixReverse;
		$('title').hidden = !$('tabs').hidden;
		$('listName').textContent = r && !program ? 'Writes' : 'List';
		$('cntList').textContent = r && !program ? num(s.Events ?? 0) : '';
		const warn = [s.Gaps ? `${s.Gaps} gap${s.Gaps === 1 ? '' : 's'}` : '', s.Lost ? `${s.Lost} lost` : ''].filter(Boolean).join(', ');
		$('cntInsns').innerHTML = program ? esc(num(s.Instructions ?? 0)) + (warn ? ` <span class="warn">${esc(warn)}</span>` : '') : '';
		for (const t of ['timeline', 'insns']) document.querySelector(`#tabs [data-tab="${t}"]`).hidden = !!r && !program;
		fitHeader();
		window.dispatchEvent(new CustomEvent('trace-result', { detail: r }));
	}

	// ---- wiring ------------------------------------------------------------------------------

	document.querySelectorAll('#form input, #form select').forEach((el) => {
		el.addEventListener('change', changed);
		if (el.tagName === 'INPUT' && el.type === 'text' || el.id === 'range' || el.id === 'dataRange' || el.id === 'stopAt') {
			el.addEventListener('input', changed);
		}
	});
	$('cfgChip').addEventListener('click', (e) => {
		e.stopPropagation();
		setEditing(!editing);
	});
	$('cfgPop').addEventListener('click', (e) => e.stopPropagation());
	$('cfgClose').addEventListener('click', () => setEditing(false));
	$('armPop').addEventListener('click', () => $('arm').click());
	// The Auto and ⋯ menus.
	function menu(btn, el) {
		btn.addEventListener('click', (e) => {
			e.stopPropagation();
			const show = el.hidden;
			document.querySelectorAll('.menu').forEach((x) => (x.hidden = true));
			el.hidden = !show;
		});
		el.addEventListener('click', (e) => e.stopPropagation());
	}
	menu($('autoBtn'), $('autoMenu'));
	menu($('moreBtn'), $('moreMenu'));
	document.addEventListener('click', () => {
		document.querySelectorAll('.menu').forEach((x) => (x.hidden = true));
		if (editing) setEditing(false);
	});
	$('export').addEventListener('click', () => ($('moreMenu').hidden = true));
	$('save').addEventListener('click', () => ($('moreMenu').hidden = true));
	window.addEventListener('reverse-changed', () => {
		$('cfgChip').disabled = !!window.aurixReverse;
		$('autoBtn').disabled = !!window.aurixReverse;
		if (window.aurixReverse) setEditing(false);
		renderSummary();
	});
	function showAuto(on, steps) {
		$('autoBtn').classList.toggle('on', on);
		$('autoText').textContent = 'Auto';
		$('autoBtn').title = on
			? `Auto is on: the trace is armed on every run${steps ? ' and step' : ''} and read on every halt, and Step Back replays it`
			: 'Auto is off: click to arm the trace on every run and read it on every halt';
	}

	$('arm').addEventListener('click', () => {
		message('Arming…', 'info');
		vscode.postMessage({ type: 'arm' });
	});
	$('stopBtn').addEventListener('click', () => vscode.postMessage({ type: 'stop' }));
	$('auto').addEventListener('change', () => vscode.postMessage({ type: 'auto', on: $('auto').checked, steps: $('autoSteps').checked }));
	$('autoSteps').addEventListener('change', () => vscode.postMessage({ type: 'auto', on: $('auto').checked, steps: $('autoSteps').checked }));
	$('export').addEventListener('click', () => vscode.postMessage({ type: 'export' }));
	$('save').addEventListener('click', () => vscode.postMessage({ type: 'save', name: $('saved').value }));
	$('saved').addEventListener('change', () => {
		if ($('saved').value) vscode.postMessage({ type: 'load', name: $('saved').value });
	});
	document.addEventListener('click', (e) => {
		const a = e.target.closest && e.target.closest('a.src');
		if (a) {
			e.preventDefault();
			openSrc(a.dataset.file, Number(a.dataset.line));
		}
	});

	function fillSaved(names, selected) {
		const sel = $('saved');
		sel.innerHTML = '<option value="">(unsaved configuration)</option>' +
			(names || []).map((n) => `<option value="${esc(n)}">${esc(n)}</option>`).join('');
		sel.value = selected || '';
	}

	window.addEventListener('message', (e) => {
		const m = e.data;
		switch (m.type) {
			case 'init':
				config = m.config || {};
				fillSaved(m.saved, m.selected);
				applyForm(config);
				// Open while there is nothing to show yet; a recording folds it.
				setEditing(!result && !armed);
				break;
			case 'saved':
				fillSaved(m.saved, m.selected);
				renderSummary();
				break;
			case 'check':
				showCheck(m);
				break;
			case 'armed':
				armed = true;
				dirty = false;
				setEditing(false);
				showResult(undefined);
				renderSummary();
				message('Armed. The core records until the stop condition; run the target.', 'info');
				setState('armed', 'armed');
				$('stopBtn').disabled = false;
				updateFormState();
				break;
			case 'status':
				showStatus(m.status, m.error);
				break;
			case 'reading':
				message('Reading and decoding the trace…', 'info');
				break;
			case 'result':
				showResult(m.result);
				break;
			case 'error':
				message(m.error, 'bad');
				break;
			case 'auto':
				$('auto').checked = !!m.on && !window.aurixReverse;
				$('autoSteps').checked = !!m.on && !!m.steps && !window.aurixReverse;
				$('autoSteps').disabled = !m.on || !!window.aurixReverse;
				showAuto($('auto').checked, $('autoSteps').checked);
				if (window.aurixReverse) break;
				if (m.error) message(m.error, 'bad');
				else if (m.on && !armed) message('Armed on every run: run the target; at the next halt the trace up to it is read here (Step Back replays it).', 'info');
				break;
		}
	});

	fitHeader();
	vscode.postMessage({ type: 'ready' });
})();
