// Trace panel webview: the Flow tab's Timeline. A program recording as calls over time (the
// server's `calls`): one timeline with the call depth as rows (a flame chart) and a row of the
// task or ISR that ran, or a band per task and ISR. Zoom with the wheel or W/S, pan by dragging
// or A/D, F fits; drag in the time ruler to measure; click a call for its details.
(function () {
	const vscode = window.traceVscode;
	const $ = (id) => document.getElementById(id);
	const L = 190; // label column
	const ROW = 18;
	const CTXROW = 22;
	const TOP_H = 54;

	const saved = (vscode.getState && vscode.getState()) || {};
	const st = {
		view: saved.flowView || 'timeline',
		layout: saved.tlLayout || 'one',
		col: saved.tlCol || 'ctx',
		v0: 0, v1: 1, sel: null, hover: null, find: '', meas: null,
	};
	let result; // the read result (Kind program)
	let data; // prepared calls
	let requested; // StoppedMs of the recording asked for
	let W = 800;
	let rows = [];

	function remember() {
		if (!vscode.setState) return;
		vscode.setState({ ...((vscode.getState && vscode.getState()) || {}), flowView: st.view, tlLayout: st.layout, tlCol: st.col });
	}

	function esc(s) {
		return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
	}
	function fmt(ns) {
		const a = Math.abs(ns);
		if (a >= 1e9) return (ns / 1e9).toFixed(3) + ' s';
		if (a >= 1e6) return (ns / 1e6).toFixed(3) + ' ms';
		if (a >= 1e5) return (ns / 1e3).toFixed(1) + ' µs';
		if (a >= 1e4) return (ns / 1e3).toFixed(2) + ' µs';
		if (a >= 1e3) return (ns / 1e3).toFixed(3) + ' µs';
		return Math.round(ns) + ' ns';
	}
	function baseName(p) {
		return String(p || '').split(/[\\/]/).pop();
	}
	function hash(s) {
		let h = 2166136261;
		for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
		return h >>> 0;
	}

	// ---- theme ------------------------------------------------------------------------------
	let theme = {};
	function readTheme() {
		const cs = getComputedStyle(document.body);
		const v = (n, d) => (cs.getPropertyValue(n) || '').trim() || d;
		const dark = !document.body.classList.contains('vscode-light') && !document.body.classList.contains('vscode-high-contrast-light');
		theme = {
			dark,
			bg: v('--vscode-editor-background', dark ? '#1e1e1e' : '#ffffff'),
			side: v('--vscode-sideBar-background', dark ? '#252526' : '#f3f3f3'),
			fg: v('--vscode-foreground', dark ? '#cccccc' : '#333333'),
			muted: v('--vscode-descriptionForeground', '#8b8b8b'),
			line: v('--vscode-panel-border', dark ? '#333333' : '#dddddd'),
			grid: dark ? 'rgba(255,255,255,.05)' : 'rgba(0,0,0,.05)',
			sel: v('--vscode-focusBorder', '#007fd4'),
			halt: v('--vscode-errorForeground', '#f14c4c'),
			meas: v('--vscode-charts-blue', '#4fc1ff'),
			find: v('--vscode-charts-yellow', '#cca700'),
			mono: v('--vscode-editor-font-family', 'Consolas, monospace'),
			font: v('--vscode-font-family', 'sans-serif'),
		};
	}
	const ISR_HUES = [335, 25, 5, 300, 45, 350, 15];
	const TASK_HUES = [212, 265, 180, 140, 240, 100, 195, 285, 160, 225];
	function ctxColor(c, shade) {
		const l0 = theme.dark ? 52 : 50;
		const l = Math.max(18, Math.min(80, l0 + (shade || 0)));
		if (c.Kind === 'isr') return `hsl(${ISR_HUES[c.k % ISR_HUES.length]},62%,${l}%)`;
		if (c.Kind === 'task') return `hsl(${TASK_HUES[c.k % TASK_HUES.length]},55%,${l}%)`;
		if (c.Kind === 'none') return `hsl(215,10%,${l - 8}%)`;
		if (c.Kind === 'unknown') return theme.dark ? '#3a3a3a' : '#d0d0d0';
		return `hsl(210,8%,${l}%)`;
	}
	function fnColor(name) {
		const h = hash(name);
		return `hsl(${h % 360},42%,${(theme.dark ? 38 : 58) + ((h >> 9) % 10)}%)`;
	}
	function byCtx() {
		return st.col === 'ctx' && data && data.tracked;
	}
	function barColor(c) {
		if (!byCtx()) return fnColor(c.fn);
		return ctxColor(data.ctxs[c.ctx], c.depth % 2 ? -9 : c.depth ? -3 : 4);
	}
	function barText() {
		return theme.dark || !byCtx() ? '#f4f4f4' : '#ffffff';
	}

	// ---- data -------------------------------------------------------------------------------
	function prepare(d) {
		const funcs = d.Functions || [];
		const files = d.Files || [];
		const ctxs = (d.Contexts || []).map((c, i) => ({ ...c, i }));
		let ki = 0, kt = 0;
		for (const c of ctxs) c.k = c.Kind === 'isr' ? ki++ : c.Kind === 'task' ? kt++ : 0;
		const tracked = d.Source === 'trace';
		const label = (c) => c.Kind === 'isr' ? c.Name : c.Kind === 'task' ? c.Name : c.Kind === 'none' ? 'OS (no task)' : c.Kind === 'unknown' ? 'unknown (before the first switch)' : 'all';
		for (const c of ctxs) c.label = label(c);
		const segs = (d.Segments || []).map((s) => ({ ctx: s[0], r0: s[1], r1: s[2], t0: s[3], t1: s[4] }));
		const segsOf = ctxs.map(() => []);
		for (const s of segs) segsOf[s.ctx].push(s);
		const calls = (d.Calls || []).map((a, id) => ({
			id, f: a[0], ctx: a[1], depth: a[2], r0: a[3], r1: a[4], t0: a[5], t1: a[6], flags: a[7], fl: a[8], line: a[9],
			fn: a[0] === null || a[0] === undefined ? '(no symbol)' : funcs[a[0]] ?? '?', pieces: [], kids: [], parent: null,
		}));
		const pieces = [];
		for (const c of calls) {
			const list = segsOf[c.ctx] || [];
			// First segment that ends at or after the call's first row.
			let lo = 0, hi = list.length;
			while (lo < hi) {
				const m = (lo + hi) >> 1;
				if (list[m].r1 < c.r0) lo = m + 1; else hi = m;
			}
			for (let k = lo; k < list.length && list[k].r0 <= c.r1; k++) {
				const s = list[k];
				const p = { call: c, t0: Math.max(c.t0, s.t0), t1: Math.min(c.t1, s.t1), seg: s };
				if (p.t1 < p.t0) p.t1 = p.t0;
				c.pieces.push(p);
				pieces.push(p);
			}
			c.run = c.pieces.reduce((a, p) => a + p.t1 - p.t0, 0);
		}
		for (const c of calls) {
			const n = c.pieces.length;
			c.pieces.forEach((p, i) => {
				p.openL = i === 0 && (c.flags & 1) !== 0;
				p.openR = i === n - 1 && (c.flags & 2) !== 0;
			});
		}
		// Parents: the open call one level up in the same task or ISR.
		const stacks = ctxs.map(() => []);
		for (const c of calls) {
			const s = stacks[c.ctx];
			while (s.length && (s[s.length - 1].r1 < c.r0 || s[s.length - 1].depth >= c.depth)) s.pop();
			const top = s[s.length - 1];
			if (top && top.depth === c.depth - 1) {
				c.parent = top;
				top.kids.push(c);
			}
			s.push(c);
		}
		for (const c of calls) c.self = Math.max(0, c.run - c.kids.reduce((a, k) => a + k.run, 0));
		const maxDepth = ctxs.map(() => 0);
		for (const c of calls) maxDepth[c.ctx] = Math.max(maxDepth[c.ctx], c.depth);
		const gaps = (d.Gaps || []).map((g) => ({ row: g[0], t0: g[1], t1: g[2] }));
		const end = d.EndNs || Math.max(1, ...calls.map((c) => c.t1));
		// Lanes: ISRs, tasks, OS, unknown; each by name.
		const rank = { isr: 0, task: 1, none: 2, unknown: 3, all: 4 };
		const order = ctxs.filter((c) => segsOf[c.i].length || calls.some((x) => x.ctx === c.i)).sort((a, b) => rank[a.Kind] - rank[b.Kind] || a.label.localeCompare(b.label)).map((c) => c.i);
		return { funcs, files, ctxs, segs, segsOf, calls, pieces, maxDepth, gaps, end, order, tracked, note: d.Note, switches: d.Switches || 0, vars: d.Variables || [] };
	}

	// ---- layout -----------------------------------------------------------------------------
	function lanesOn() {
		return st.layout === 'lanes' && data && data.tracked;
	}
	function layoutRows() {
		rows = [];
		let y = 4;
		if (!lanesOn()) {
			if (data.tracked) {
				rows.push({ type: 'ctx', y, h: CTXROW });
				y += CTXROW + 6;
			}
			const md = Math.max(0, ...data.maxDepth);
			for (let d = 0; d <= md; d++) {
				rows.push({ type: 'depth', d, y, h: ROW });
				y += ROW;
			}
		} else {
			for (const k of data.order) {
				const h = 10 + (data.maxDepth[k] + 1) * ROW + 6;
				rows.push({ type: 'lane', ctx: k, y, h: Math.max(h, 52) });
				y += Math.max(h, 52);
			}
		}
		return y + 6;
	}
	function rowOf(c) {
		for (const r of rows) {
			if (r.type === 'depth' && r.d === c.depth) return { y: r.y, h: ROW };
			if (r.type === 'lane' && r.ctx === c.ctx) return { y: r.y + 10 + c.depth * ROW, h: ROW };
		}
		return null;
	}

	// ---- drawing helpers --------------------------------------------------------------------
	const X = (t) => L + ((t - st.v0) / (st.v1 - st.v0)) * (W - L);
	const T = (x) => st.v0 + ((x - L) / (W - L)) * (st.v1 - st.v0);
	function setup(cv, h) {
		const dpr = window.devicePixelRatio || 1;
		cv.style.height = h + 'px';
		cv.width = Math.round(W * dpr);
		cv.height = Math.round(h * dpr);
		const g = cv.getContext('2d');
		g.setTransform(dpr, 0, 0, dpr, 0, 0);
		return g;
	}
	function hatch(g, x0, y0, x1, y1, col) {
		g.save();
		g.beginPath();
		g.rect(x0, y0, x1 - x0, y1 - y0);
		g.clip();
		g.strokeStyle = col;
		g.lineWidth = 1;
		for (let x = x0 - (y1 - y0); x < x1; x += 6) {
			g.beginPath();
			g.moveTo(x, y1);
			g.lineTo(x + (y1 - y0), y0);
			g.stroke();
		}
		g.restore();
	}
	function clipText(g, s, w) {
		if (w <= 8) return '';
		if (g.measureText(s).width <= w) return s;
		let lo = 0, hi = s.length;
		while (lo < hi) {
			const m = (lo + hi + 1) >> 1;
			if (g.measureText(s.slice(0, m) + '…').width <= w) lo = m; else hi = m - 1;
		}
		return lo > 1 ? s.slice(0, lo) + '…' : '';
	}
	function niceStep(span, px) {
		const raw = (span * 90) / px;
		const p = Math.pow(10, Math.floor(Math.log10(raw)));
		return [1, 2, 5, 10].map((m) => m * p).find((s) => s >= raw);
	}
	// Ruler label: the unit from the visible span (ns, µs, ms, s), the decimals from the tick step.
	function tickLabel(t, step, span) {
		const [div, unit] = span >= 2e9 ? [1e9, 's'] : span >= 2e6 ? [1e6, 'ms'] : span >= 2e3 ? [1e3, 'µs'] : [1, 'ns'];
		const dec = Math.min(6, Math.max(0, -Math.floor(Math.log10(step / div) + 1e-9)));
		return (t / div).toFixed(dec) + ' ' + unit;
	}
	function matches(c) {
		return !st.find || c.fn.toLowerCase().includes(st.find);
	}

	// ---- top: overview + ruler --------------------------------------------------------------
	function drawTop() {
		const g = setup($('tlTop'), TOP_H);
		g.fillStyle = theme.bg;
		g.fillRect(0, 0, W, TOP_H);
		g.fillStyle = theme.side;
		g.fillRect(0, 0, L, TOP_H);
		g.font = `11px ${theme.font}`;
		g.fillStyle = theme.muted;
		g.fillText('Whole recording', 10, 17);
		g.fillText('Time from the first stamp', 10, 45);
		const ox = (t) => L + (t / data.end) * (W - L - 8);
		const oy = 6, oh = 16;
		if (data.tracked) {
			for (const s of data.segs) {
				g.fillStyle = byCtx() ? ctxColor(data.ctxs[s.ctx]) : theme.line;
				g.fillRect(ox(s.t0), oy, Math.max(1, ox(s.t1) - ox(s.t0)), oh);
			}
		} else {
			for (const p of data.pieces) {
				if (p.call.depth !== 0) continue;
				g.fillStyle = fnColor(p.call.fn);
				g.fillRect(ox(p.t0), oy, Math.max(1, ox(p.t1) - ox(p.t0)), oh);
			}
		}
		for (const gp of data.gaps) hatch(g, ox(gp.t0), oy, Math.max(ox(gp.t1), ox(gp.t0) + 2), oy + oh, theme.muted);
		const a = ox(Math.max(0, st.v0)), b = ox(Math.min(data.end, st.v1));
		g.fillStyle = theme.dark ? 'rgba(0,0,0,.55)' : 'rgba(255,255,255,.6)';
		g.fillRect(L, oy - 2, a - L, oh + 4);
		g.fillRect(b, oy - 2, W - b, oh + 4);
		g.strokeStyle = theme.fg;
		g.lineWidth = 1;
		g.strokeRect(a + 0.5, oy - 2.5, Math.max(3, b - a) - 1, oh + 5);
		if (haltShown()) {
			g.fillStyle = theme.halt;
			g.fillRect(ox(data.end) - 1, oy - 3, 2, oh + 6);
		}
		// Ruler.
		const ry = 28;
		g.strokeStyle = theme.line;
		g.beginPath();
		g.moveTo(0, ry - 0.5);
		g.lineTo(W, ry - 0.5);
		g.stroke();
		const step = niceStep(st.v1 - st.v0, W - L);
		g.fillStyle = theme.muted;
		g.font = `11px ${theme.mono}`;
		for (let t = Math.ceil(st.v0 / step) * step; t <= st.v1; t += step) {
			const x = Math.round(X(t)) + 0.5;
			if (x < L) continue;
			g.strokeStyle = theme.muted;
			g.beginPath();
			g.moveTo(x, 46);
			g.lineTo(x, 54);
			g.stroke();
			g.fillText(tickLabel(t, step, st.v1 - st.v0), x + 3, 44);
		}
		if (haltShown() && data.end >= st.v0 && data.end <= st.v1 * 1.0001) {
			const x = Math.min(X(data.end), W - 1);
			g.fillStyle = theme.halt;
			g.fillRect(x - 1, ry, 2, 26);
			g.font = `600 11px ${theme.font}`;
			const w = g.measureText('halt').width;
			g.fillText('halt', Math.min(x + 4, W - w - 4), ry + 11);
		}
		if (st.meas) {
			const m0 = Math.min(st.meas.a, st.meas.b), m1 = Math.max(st.meas.a, st.meas.b);
			const x0 = Math.max(L, X(m0)), x1 = Math.min(W, X(m1));
			g.globalAlpha = 0.2;
			g.fillStyle = theme.meas;
			g.fillRect(x0, ry, x1 - x0, 26);
			g.globalAlpha = 1;
			g.fillRect(x0, ry, 1, 26);
			g.fillRect(x1 - 1, ry, 1, 26);
			g.font = `600 11px ${theme.font}`;
			const s = 'Δ ' + fmt(m1 - m0);
			const w = g.measureText(s).width;
			g.fillStyle = theme.bg;
			g.fillRect((x0 + x1) / 2 - w / 2 - 4, ry + 1, w + 8, 14);
			g.fillStyle = theme.meas;
			g.fillText(s, (x0 + x1) / 2 - w / 2, ry + 12);
		}
	}

	function haltShown() {
		return result && result.Halt && result.ReachesHalt && !result.RanSince;
	}

	// ---- lanes ------------------------------------------------------------------------------
	function drawLanes() {
		const scroll = $('tlScroll');
		const H = Math.max(layoutRows(), scroll.clientHeight - 2);
		const g = setup($('tlLanes'), H);
		g.fillStyle = theme.bg;
		g.fillRect(0, 0, W, H);
		const step = niceStep(st.v1 - st.v0, W - L);
		g.strokeStyle = theme.grid;
		for (let t = Math.ceil(st.v0 / step) * step; t <= st.v1; t += step) {
			const x = Math.round(X(t)) + 0.5;
			if (x > L) {
				g.beginPath();
				g.moveTo(x, 0);
				g.lineTo(x, H);
				g.stroke();
			}
		}
		// Labels.
		g.fillStyle = theme.side;
		g.fillRect(0, 0, L, H);
		for (const r of rows) {
			if (r.type === 'ctx') {
				g.font = `11px ${theme.font}`;
				g.fillStyle = theme.fg;
				g.fillText('Task / ISR', 10, r.y + 15);
			} else if (r.type === 'depth') {
				g.font = `11px ${theme.font}`;
				g.fillStyle = theme.muted;
				g.fillText(r.d === 0 ? 'Call depth 0' : String(r.d), r.d === 0 ? 10 : 74, r.y + 13);
			} else {
				const c = data.ctxs[r.ctx];
				g.strokeStyle = theme.line;
				g.beginPath();
				g.moveTo(0, r.y + r.h - 0.5);
				g.lineTo(W, r.y + r.h - 0.5);
				g.stroke();
				g.fillStyle = ctxColor(c);
				g.fillRect(10, r.y + 9, 9, 9);
				g.font = `600 12px ${theme.font}`;
				g.fillStyle = theme.fg;
				g.fillText(clipText(g, c.label, L - 34), 25, r.y + 18);
				g.font = `11px ${theme.font}`;
				g.fillStyle = theme.muted;
				const run = data.segsOf[r.ctx].reduce((a, s) => a + s.t1 - s.t0, 0);
				g.fillText(clipText(g, `${c.Kind === 'isr' ? 'ISR' : c.Kind === 'task' ? 'Task' : ''} ${((100 * run) / data.end).toFixed(1)} % · ${fmt(run)}`.trim(), L - 34), 25, r.y + 33);
			}
		}
		g.strokeStyle = theme.line;
		g.beginPath();
		g.moveTo(L - 0.5, 0);
		g.lineTo(L - 0.5, H);
		g.stroke();

		g.save();
		g.beginPath();
		g.rect(L, 0, W - L, H);
		g.clip();
		// The task / ISR row.
		const cr = rows.find((r) => r.type === 'ctx');
		if (cr) {
			for (const s of data.segs) {
				const x0 = X(s.t0), x1 = X(s.t1);
				if (x1 < L || x0 > W) continue;
				const c = data.ctxs[s.ctx];
				g.fillStyle = ctxColor(c);
				g.fillRect(x0, cr.y, Math.max(1, x1 - x0), cr.h);
				if (c.Kind === 'unknown') hatch(g, x0, cr.y, x1, cr.y + cr.h, theme.muted);
				if (c.Kind === 'isr') {
					g.fillStyle = 'rgba(255,255,255,.6)';
					g.fillRect(x0, cr.y, Math.max(1, x1 - x0), 2);
				}
				if (st.sel && st.sel.ctx === s.ctx && s.r1 >= st.sel.r0 && s.r0 <= st.sel.r1) {
					g.strokeStyle = theme.sel;
					g.lineWidth = 1;
					g.strokeRect(x0 + 0.5, cr.y + 0.5, Math.max(1, x1 - x0 - 1), cr.h - 1);
				}
				g.font = `600 11px ${theme.font}`;
				g.fillStyle = c.Kind === 'unknown' ? theme.fg : '#ffffff';
				const t = clipText(g, (c.Kind === 'isr' ? 'ISR ' : '') + c.label, x1 - Math.max(x0, L) - 8);
				if (t) g.fillText(t, Math.max(x0, L) + 4, cr.y + 15);
			}
		}
		// Preempted: dashed outlines between the pieces of a call (lanes).
		if (lanesOn()) {
			g.setLineDash([3, 3]);
			g.lineWidth = 1;
			for (const c of data.calls) {
				if (c.pieces.length < 2) continue;
				const r = rowOf(c);
				if (!r) continue;
				for (let i = 1; i < c.pieces.length; i++) {
					const a = c.pieces[i - 1], b = c.pieces[i];
					const x0 = X(a.t1), x1 = X(b.t0);
					if (x1 < L || x0 > W || x1 - x0 < 1) continue;
					g.strokeStyle = ctxColor(data.ctxs[c.ctx]);
					g.strokeRect(x0 + 0.5, r.y + 2.5, x1 - x0 - 1, r.h - 5);
				}
			}
			g.setLineDash([]);
		}
		// Calls.
		g.font = `11px ${theme.mono}`;
		const selFn = st.sel && st.sel.fn;
		const px = (st.v1 - st.v0) / (W - L);
		for (const p of data.pieces) {
			if (p.t1 < st.v0 - px || p.t0 > st.v1 + px) continue;
			const c = p.call;
			const r = rowOf(c);
			if (!r) continue;
			const x0 = X(p.t0), x1 = X(p.t1);
			const w = Math.max(1, x1 - x0);
			g.globalAlpha = matches(c) ? 1 : 0.22;
			g.fillStyle = barColor(c);
			g.fillRect(x0, r.y + 1, w, r.h - 2);
			if (w > 4) {
				g.fillStyle = 'rgba(0,0,0,.3)';
				g.fillRect(x0 + w - 1, r.y + 1, 1, r.h - 2);
			}
			g.fillStyle = 'rgba(255,255,255,.8)';
			if (p.openL && w > 6) {
				g.beginPath();
				g.moveTo(x0 + 1, r.y + 1);
				g.lineTo(x0 + 5, r.y + r.h / 2);
				g.lineTo(x0 + 1, r.y + r.h - 1);
				g.fill();
			}
			if (p.openR && w > 6) {
				g.beginPath();
				g.moveTo(x0 + w - 1, r.y + 1);
				g.lineTo(x0 + w - 5, r.y + r.h / 2);
				g.lineTo(x0 + w - 1, r.y + r.h - 1);
				g.fill();
			}
			if (w > 26) {
				const s = clipText(g, c.fn, Math.min(x1, W) - Math.max(x0, L) - 10 - (p.openL ? 4 : 0));
				if (s) {
					g.fillStyle = barText();
					g.fillText(s, Math.max(x0, L) + 4 + (p.openL ? 4 : 0), r.y + 13);
				}
			}
			g.globalAlpha = 1;
			if (st.find && matches(c)) {
				g.strokeStyle = theme.find;
				g.lineWidth = 1;
				g.strokeRect(x0 + 0.5, r.y + 1.5, w - 1, r.h - 3);
			} else if (selFn && c.fn === selFn && c !== st.sel) {
				g.fillStyle = theme.fg;
				g.fillRect(x0, r.y + r.h - 3, w, 2);
			}
			if (c === st.hover && c !== st.sel) {
				g.strokeStyle = theme.fg;
				g.lineWidth = 1;
				g.strokeRect(x0 + 0.5, r.y + 1.5, w - 1, r.h - 3);
			}
		}
		if (st.sel) {
			const r = rowOf(st.sel);
			if (r) {
				g.strokeStyle = theme.sel;
				g.lineWidth = 2;
				for (const p of st.sel.pieces) g.strokeRect(X(p.t0) + 1, r.y + 1, Math.max(2, X(p.t1) - X(p.t0) - 2), r.h - 2);
			}
		}
		// Gaps: code ran that the trace does not show.
		for (const gp of data.gaps) {
			const x0 = X(gp.t0), x1 = Math.max(X(gp.t1), X(gp.t0) + 3);
			if (x1 < L || x0 > W) continue;
			g.fillStyle = theme.bg;
			g.fillRect(x0, 0, x1 - x0, H);
			hatch(g, x0, 0, x1, H, theme.line);
		}
		if (haltShown() && data.end >= st.v0 && data.end <= st.v1 * 1.0001) {
			const x = Math.min(X(data.end), W - 1);
			g.fillStyle = theme.halt;
			g.fillRect(x - 1, 0, 2, H);
		}
		if (st.meas) {
			const a = Math.min(st.meas.a, st.meas.b), b = Math.max(st.meas.a, st.meas.b);
			g.globalAlpha = 0.08;
			g.fillStyle = theme.meas;
			g.fillRect(X(a), 0, X(b) - X(a), H);
			g.globalAlpha = 0.7;
			g.fillRect(X(a), 0, 1, H);
			g.fillRect(X(b) - 1, 0, 1, H);
			g.globalAlpha = 1;
		}
		g.restore();
	}

	// ---- side panel -------------------------------------------------------------------------
	function shareRows(items, total) {
		return '<div class="share">' + items.map(([name, v, col, mono]) =>
			`<span class="n${mono ? ' mono' : ''}" title="${esc(name)}">${esc(name)}</span>` +
			`<span class="track"><span data-pct="${((100 * v) / (total || 1)).toFixed(1)}" data-col="${esc(col)}"></span></span>` +
			`<span class="p">${((100 * v) / (total || 1)).toFixed(1)} %</span>`).join('') + '</div>';
	}
	function windowStats(a, b) {
		const ctx = new Map(), fns = new Map();
		for (const s of data.segs) {
			const d = Math.min(b, s.t1) - Math.max(a, s.t0);
			if (d > 0) ctx.set(s.ctx, (ctx.get(s.ctx) || 0) + d);
		}
		const ov = (c) => c.pieces.reduce((s, p) => s + Math.max(0, Math.min(b, p.t1) - Math.max(a, p.t0)), 0);
		for (const c of data.calls) {
			if (c.t1 < a || c.t0 > b) continue;
			const self = ov(c) - c.kids.reduce((s, k) => s + ov(k), 0);
			if (self > 0) {
				const e = fns.get(c.fn) || { v: 0, ctx: c.ctx };
				e.v += self;
				fns.set(c.fn, e);
			}
		}
		return { ctx, fns };
	}
	function renderSide() {
		const side = $('tlSide');
		let h = '';
		const c = st.sel;
		if (c) {
			const path = [];
			for (let p = c.parent; p; p = p.parent) path.unshift(p);
			const file = c.fl !== null && c.fl !== undefined ? data.files[c.fl] : undefined;
			const last = c.pieces[c.pieces.length - 1];
			const inter = [];
			for (let i = 1; i < c.pieces.length; i++) {
				const a = c.pieces[i - 1], b = c.pieces[i];
				const by = data.segs.filter((s) => s.t0 >= a.t1 - 0.01 && s.t1 <= b.t0 + 0.01 && s.ctx !== c.ctx).map((s) => data.ctxs[s.ctx].label);
				const gap = data.gaps.some((g) => g.t0 >= a.t1 - 0.01 && g.t1 <= b.t0 + 0.01);
				inter.push(`${esc([...new Set(by)].join(', ') || (gap ? 'gap in the trace' : 'another task / ISR'))} at ${fmt(a.t1)} for ${fmt(b.t0 - a.t1)}`);
			}
			const all = data.calls.filter((x) => x.fn === c.fn);
			const runs = all.map((x) => x.run);
			const inCtx = new Map();
			for (const x of all) inCtx.set(x.ctx, (inCtx.get(x.ctx) || 0) + 1);
			h += `<h3>Selected call</h3><div class="fnname">${esc(c.fn)}</div>`;
			if (data.tracked) h += `<div class="ctxchip"><i data-col="${esc(ctxColor(data.ctxs[c.ctx]))}"></i>${esc(data.ctxs[c.ctx].label)}</div>`;
			if (c.flags & 4) h += `<div class="hint">entered by an interrupt</div>`;
			if (c.flags & 8) h += `<div class="hint">entered by a trap (syscall)</div>`;
			h += `<h3>Called from</h3><div class="path">${path.map((p) => `<a href="#" data-call="${p.id}">${esc(p.fn)}</a>`).join(' ›<br>')}${path.length ? ' ›<br>' : (c.flags & 1 ? '<span class="hint">(began before the recording)</span><br>' : '')}<span class="cur">${esc(c.fn)}</span></div>`;
			h += `<h3>This call</h3><div class="kv">` +
				`<span>Start</span><span class="mono">${fmt(c.t0)}${c.flags & 1 ? ' <span class="hint">(earlier)</span>' : ''}</span>` +
				`<span>End</span><span class="mono">${last && last.openR ? (haltShown() && c.r1 === lastRow() ? '<span class="bad">running at the halt</span>' : 'still running') : fmt(c.t1)}</span>` +
				`<span>Wall time</span><span class="mono">${fmt(c.t1 - c.t0)}</span>` +
				`<span>Run time</span><span class="mono">${fmt(c.run)}${c.pieces.length > 1 ? ' <span class="hint">(without interruptions)</span>' : ''}</span>` +
				`<span>Self time</span><span class="mono">${fmt(c.self)}</span>` +
				`<span>Instructions</span><span class="mono">${(c.r1 - c.r0 + 1).toLocaleString('en')} rows</span>` +
				`<span>Source</span><span>${file ? `<a href="#" class="src" data-file="${esc(file)}" data-line="${c.line || 0}">${esc(baseName(file))}:${c.line || '?'}</a>` : '<span class="hint">no line information</span>'}</span>` +
				`</div>`;
			if (inter.length) h += `<h3>Interrupted</h3><div class="small">${inter.join('<br>')}</div>`;
			h += `<h3>All ${all.length} calls of ${esc(c.fn)}</h3><div class="kv">` +
				`<span>Run time</span><span class="mono">min ${fmt(Math.min(...runs))} · max ${fmt(Math.max(...runs))}</span>` +
				`<span>Total</span><span class="mono">${fmt(runs.reduce((a, b) => a + b, 0))}</span>` +
				(data.tracked ? `<span>In</span><span>${[...inCtx].map(([k, n]) => `${esc(data.ctxs[k].label)} ×${n}`).join(', ')}</span>` : '') +
				`</div>`;
			h += `<div class="btns">` +
				(file ? `<button type="button" id="tlSrc">Go to source</button>` : '') +
				`<button type="button" class="secondary" id="tlInsns" title="The Instructions tab at the call's first instruction">Instructions</button>` +
				(window.aurixReverse ? '' : `<button type="button" class="secondary" id="tlReplay" title="Show the call's first instruction in the debugger: source, registers, locals, call stack">Show in debugger</button>`) +
				`<button type="button" class="secondary" id="tlZoomCall">Zoom to call</button></div>`;
		}
		const [a, b] = st.meas ? [Math.min(st.meas.a, st.meas.b), Math.max(st.meas.a, st.meas.b)] : [Math.max(0, st.v0), Math.min(data.end, st.v1)];
		const span = Math.max(1e-6, b - a);
		const ws = windowStats(a, b);
		h += `<h3>${st.meas ? 'Measured range' : 'In view'} · <span class="nocase">${fmt(span)}</span>${st.meas ? ' <a href="#" id="tlClearMeas" class="nocase">clear</a>' : ''}</h3>`;
		if (data.tracked) {
			h += shareRows(data.order.filter((k) => ws.ctx.get(k)).map((k) => [data.ctxs[k].label, ws.ctx.get(k), ctxColor(data.ctxs[k])]), span);
		}
		const gap = data.gaps.reduce((s, g) => s + Math.max(0, Math.min(b, g.t1) - Math.max(a, g.t0)), 0);
		if (gap > 0) h += `<div class="hint">${fmt(gap)} not in the trace (gap)</div>`;
		h += `<h3>Self time by function</h3>`;
		const top = [...ws.fns].sort((x, y) => y[1].v - x[1].v).slice(0, 10);
		h += shareRows(top.map(([fn, e]) => [fn, e.v, byCtx() ? ctxColor(data.ctxs[e.ctx]) : fnColor(fn), true]), span);
		if (!c) h += `<div class="hint top">Click a call for its details, double-click to zoom to it. Drag in the time ruler to measure.</div>`;
		side.innerHTML = h;
		// Colours and widths through the DOM: the webview's CSP drops inline style attributes.
		side.querySelectorAll('[data-col]').forEach((el) => {
			el.style.background = el.dataset.col;
			if (el.dataset.pct) el.style.width = el.dataset.pct + '%';
		});
		side.querySelectorAll('[data-call]').forEach((el) => el.addEventListener('click', (e) => {
			e.preventDefault();
			select(data.calls[Number(el.dataset.call)]);
		}));
		const on = (id, f) => $(id) && $(id).addEventListener('click', (e) => {
			e.preventDefault();
			f();
		});
		on('tlSrc', () => {
			const f = data.files[st.sel.fl];
			if (f) vscode.postMessage({ type: 'open', file: f, line: st.sel.line });
		});
		on('tlInsns', () => window.traceGotoRow && window.traceGotoRow(st.sel.r0));
		on('tlReplay', () => vscode.postMessage({ type: 'replayAt', at: Math.max(0, st.sel.r0 - 1) }));
		on('tlZoomCall', () => zoomTo(st.sel));
		on('tlClearMeas', () => {
			st.meas = null;
			redraw();
		});
	}
	function lastRow() {
		return data.calls.reduce((m, c) => Math.max(m, c.r1), 0);
	}

	function renderLegend() {
		let h = '';
		if (data.tracked) {
			h += data.order.map((k) => `<span class="chip"><i data-col="${esc(ctxColor(data.ctxs[k]))}"></i>${esc(data.ctxs[k].label)}</span>`).join('');
		} else {
			h += `<span>Colour by function</span>`;
		}
		h += `<span class="chip"><i class="hatch"></i>gap</span>`;
		if (haltShown()) h += `<span class="chip"><i class="halt"></i>halt</span>`;
		if (lanesOn()) h += `<span class="chip"><i class="dashed"></i>interrupted</span>`;
		h += `<span class="keys"><kbd>W</kbd><kbd>S</kbd> zoom · <kbd>A</kbd><kbd>D</kbd> pan · wheel zoom · drag pan · <kbd>F</kbd> fit</span>`;
		$('tlLegend').innerHTML = h;
		$('tlLegend').querySelectorAll('[data-col]').forEach((el) => (el.style.background = el.dataset.col));
		const tog = (id, v) => $(id).classList.toggle('on', v);
		tog('tlLayoutOne', !lanesOn());
		tog('tlLayoutLanes', lanesOn());
		tog('tlColCtx', byCtx());
		tog('tlColFn', !byCtx());
		$('tlLayoutLanes').disabled = !data.tracked;
		$('tlColCtx').disabled = !data.tracked;
		// What the Task / ISR row is based on.
		const note = $('flowNote');
		if (data.tracked) {
			note.textContent = ` Task / ISR from ${data.vars.map((v) => v.Name).join(', ')}: ${data.switches} switch${data.switches === 1 ? '' : 'es'} recorded`;
		} else if (data.note) {
			note.textContent = ' ' + data.note;
		} else {
			note.textContent = ' Task / ISR not recorded: tick "with the task / ISR (ORTI)" and arm again';
		}
	}

	// ---- frame ------------------------------------------------------------------------------
	let raf = 0, sideDirty = true;
	function redraw(side = true) {
		sideDirty = sideDirty || side;
		if (raf || !data || $('timeline').hidden) return;
		raf = requestAnimationFrame(() => {
			raf = 0;
			W = Math.max(400, $('tlLeft').clientWidth);
			drawTop();
			drawLanes();
			$('tlView').textContent = `${fmt(Math.max(0, st.v0))} … ${fmt(Math.min(data.end, st.v1))}`;
			if (sideDirty) {
				renderSide();
				sideDirty = false;
			}
		});
	}
	function clampView(v0, v1) {
		const end = data.end;
		const span = Math.max(20, Math.min(end * 1.1, v1 - v0));
		const lo = -end * 0.03, hi = end * 1.03;
		if (v0 < lo) v0 = lo;
		if (v0 + span > hi) v0 = hi - span;
		st.v0 = v0;
		st.v1 = v0 + span;
	}
	function zoom(f, at) {
		if (at === undefined) at = (st.v0 + st.v1) / 2;
		clampView(at - (at - st.v0) * f, at + (st.v1 - at) * f);
		redraw();
	}
	function pan(dt) {
		clampView(st.v0 + dt, st.v1 + dt);
		redraw();
	}
	function fit() {
		clampView(0, data.end);
		redraw();
	}
	function zoomTo(c) {
		if (!c) return;
		const m = (c.t1 - c.t0) * 0.15 + 20;
		clampView(c.t0 - m, c.t1 + m);
		redraw();
	}
	function select(c) {
		st.sel = c || null;
		redraw();
	}

	// ---- hit test, tooltip ------------------------------------------------------------------
	function hit(x, y) {
		if (x < L || !data) return null;
		const t = T(x), tol = ((st.v1 - st.v0) / (W - L)) * 2;
		let best = null;
		for (const p of data.pieces) {
			if (t < p.t0 - tol || t > p.t1 + tol) continue;
			const r = rowOf(p.call);
			if (r && y >= r.y && y < r.y + r.h) {
				// The narrowest under the pointer (a short call next to a long one).
				if (!best || p.t1 - p.t0 < best.piece.t1 - best.piece.t0) best = { call: p.call, piece: p };
			}
		}
		if (best) return best;
		const cr = rows.find((r) => r.type === 'ctx');
		if (cr && y >= cr.y && y < cr.y + cr.h) {
			const s = data.segs.find((s) => t >= s.t0 && t <= s.t1);
			if (s) return { seg: s };
		}
		return null;
	}
	function showTip(e, h) {
		const tip = $('tlTip');
		if (!h) {
			tip.style.display = 'none';
			return;
		}
		let html;
		if (h.seg) {
			const c = data.ctxs[h.seg.ctx];
			html = `<div class="f">${esc((c.Kind === 'isr' ? 'ISR ' : c.Kind === 'task' ? 'Task ' : '') + c.label)}</div>` +
				`<div class="r"><span>From</span><span class="mono">${fmt(h.seg.t0)}</span><span>For</span><span class="mono">${fmt(h.seg.t1 - h.seg.t0)}</span></div>`;
		} else {
			const c = h.call;
			const file = c.fl !== null && c.fl !== undefined ? data.files[c.fl] : undefined;
			html = `<div class="f">${esc(c.fn)}</div><div class="r">` +
				(data.tracked ? `<span>Task / ISR</span><span>${esc(data.ctxs[c.ctx].label)}</span>` : '') +
				(file ? `<span>Source</span><span class="mono">${esc(baseName(file))}:${c.line || '?'}</span>` : '') +
				`<span>Start</span><span class="mono">${fmt(c.t0)}</span>` +
				`<span>Run time</span><span class="mono">${fmt(c.run)}${c.pieces.length > 1 ? ` (wall ${fmt(c.t1 - c.t0)})` : ''}</span>` +
				`<span>Self time</span><span class="mono">${fmt(c.self)}</span>` +
				`<span>Depth</span><span class="mono">${c.depth}</span></div>` +
				(c.flags & 1 ? '<div class="w">began before the recording: start and run time are lower bounds</div>' : '') +
				(c.flags & 2 ? '<div class="w">still running at the end of the recording</div>' : '') +
				(c.pieces.length > 1 ? `<div class="w">interrupted ${c.pieces.length - 1}×</div>` : '');
		}
		tip.innerHTML = html;
		tip.style.display = 'block';
		const r = tip.getBoundingClientRect();
		let x = e.clientX + 14, y = e.clientY + 16;
		if (x + r.width > innerWidth - 8) x = e.clientX - r.width - 14;
		if (y + r.height > innerHeight - 8) y = e.clientY - r.height - 12;
		tip.style.left = x + 'px';
		tip.style.top = y + 'px';
	}

	// ---- input ------------------------------------------------------------------------------
	const lanes = $('tlLanes'), top = $('tlTop');
	let drag = null;
	const localX = (e, el) => e.clientX - el.getBoundingClientRect().left;
	const localY = (e, el) => e.clientY - el.getBoundingClientRect().top;
	lanes.addEventListener('mousedown', (e) => {
		if (e.button !== 0) return;
		drag = { x: localX(e, lanes), v0: st.v0, v1: st.v1, moved: false };
	});
	window.addEventListener('mousemove', (e) => {
		if (!drag || drag.top) return;
		const dx = localX(e, lanes) - drag.x;
		if (Math.abs(dx) > 3) {
			drag.moved = true;
			lanes.classList.add('pan');
			$('tlTip').style.display = 'none';
		}
		if (drag.moved) {
			const dt = (-dx / (W - L)) * (drag.v1 - drag.v0);
			clampView(drag.v0 + dt, drag.v1 + dt);
			redraw(false);
		}
	});
	window.addEventListener('mouseup', (e) => {
		if (drag && !drag.top && !drag.moved && e.target === lanes) {
			const h = hit(localX(e, lanes), localY(e, lanes));
			select(h && h.call);
		}
		if (drag && drag.moved) redraw();
		if (drag && !drag.top) drag = null;
		lanes.classList.remove('pan');
	});
	lanes.addEventListener('mousemove', (e) => {
		if (drag && drag.moved) return;
		const h = hit(localX(e, lanes), localY(e, lanes));
		const hv = (h && h.call) || null;
		if (hv !== st.hover) {
			st.hover = hv;
			redraw(false);
		}
		lanes.style.cursor = h ? 'pointer' : 'default';
		showTip(e, h);
	});
	lanes.addEventListener('mouseleave', () => {
		$('tlTip').style.display = 'none';
		if (st.hover) {
			st.hover = null;
			redraw(false);
		}
	});
	lanes.addEventListener('dblclick', (e) => {
		const h = hit(localX(e, lanes), localY(e, lanes));
		if (h && h.call) {
			select(h.call);
			zoomTo(h.call);
		}
	});
	const wheel = (e) => {
		if (!data) return;
		e.preventDefault();
		const x = localX(e, lanes);
		if (e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) pan(((e.deltaX || e.deltaY) / (W - L)) * (st.v1 - st.v0));
		else zoom(Math.exp(e.deltaY * 0.0018), x > L ? T(x) : undefined);
	};
	lanes.addEventListener('wheel', wheel, { passive: false });
	top.addEventListener('wheel', wheel, { passive: false });
	top.addEventListener('mousedown', (e) => {
		if (!data || e.button !== 0) return;
		const x = localX(e, top), y = localY(e, top);
		if (x < L) return;
		if (y < 26) {
			// The overview: move the view there.
			const go = (px) => {
				const t = ((px - L) / (W - L - 8)) * data.end, s = st.v1 - st.v0;
				clampView(t - s / 2, t + s / 2);
				redraw();
			};
			go(x);
			drag = { top: 'ov' };
			const mv = (ev) => go(localX(ev, top));
			const up = () => {
				removeEventListener('mousemove', mv);
				removeEventListener('mouseup', up);
				drag = null;
			};
			addEventListener('mousemove', mv);
			addEventListener('mouseup', up);
		} else {
			// The ruler: measure.
			const t0 = T(x);
			st.meas = { a: t0, b: t0 };
			drag = { top: 'meas' };
			const mv = (ev) => {
				st.meas.b = T(localX(ev, top));
				redraw();
			};
			const up = () => {
				removeEventListener('mousemove', mv);
				removeEventListener('mouseup', up);
				drag = null;
				if (Math.abs(st.meas.b - st.meas.a) < (st.v1 - st.v0) / 400) st.meas = null;
				redraw();
			};
			addEventListener('mousemove', mv);
			addEventListener('mouseup', up);
		}
	});
	window.addEventListener('keydown', (e) => {
		if (!data || $('timeline').hidden || $('flowPane').hidden) return;
		if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement || e.ctrlKey || e.metaKey || e.altKey) return;
		const k = e.key.toLowerCase(), s = st.v1 - st.v0;
		if (k === 'w') zoom(0.8);
		else if (k === 's') zoom(1.25);
		else if (k === 'a') pan(-s * 0.15);
		else if (k === 'd') pan(s * 0.15);
		else if (k === 'f') fit();
		else if (k === 'escape') {
			st.sel = null;
			st.meas = null;
			redraw();
		} else return;
		e.preventDefault();
	});
	$('tlIn').addEventListener('click', () => zoom(0.7));
	$('tlOut').addEventListener('click', () => zoom(1.4));
	$('tlFit').addEventListener('click', fit);
	$('tlFind').addEventListener('input', (e) => {
		st.find = e.target.value.trim().toLowerCase();
		redraw(false);
	});
	const mode = (key, value) => () => {
		st[key] = value;
		remember();
		if (data) renderLegend();
		redraw();
	};
	$('tlLayoutOne').addEventListener('click', mode('layout', 'one'));
	$('tlLayoutLanes').addEventListener('click', mode('layout', 'lanes'));
	$('tlColCtx').addEventListener('click', mode('col', 'ctx'));
	$('tlColFn').addEventListener('click', mode('col', 'fn'));
	window.addEventListener('resize', () => redraw());
	new MutationObserver(() => {
		readTheme();
		redraw();
	}).observe(document.body, { attributes: true, attributeFilter: ['class'] });

	// ---- the Flow tab's views ---------------------------------------------------------------
	function setView(v) {
		st.view = v;
		remember();
		document.querySelectorAll('#flowBar [data-view]').forEach((b) => b.classList.toggle('on', b.dataset.view === v));
		const program = result && result.Kind === 'program';
		$('flowPane').classList.toggle('timeline', program && v === 'timeline');
		$('timeline').hidden = !(program && v === 'timeline');
		if (program && v === 'timeline') {
			request();
			redraw();
		}
	}
	document.querySelectorAll('#flowBar [data-view]').forEach((b) => b.addEventListener('click', () => setView(b.dataset.view)));

	/** Asks for the calls of the shown recording (once per recording). */
	function request() {
		if (!result || result.Kind !== 'program' || requested === result.StoppedMs) return;
		requested = result.StoppedMs;
		$('tlSide').innerHTML = '<div class="hint">Reading the calls…</div>';
		vscode.postMessage({ type: 'calls', stoppedMs: result.StoppedMs });
	}

	window.addEventListener('trace-result', (e) => {
		result = e.detail;
		data = undefined;
		requested = undefined;
		st.sel = null;
		st.hover = null;
		st.meas = null;
		const program = result && result.Kind === 'program';
		$('flowBar').hidden = !program;
		$('flowNote').textContent = '';
		setView(st.view);
	});

	window.addEventListener('message', (e) => {
		const m = e.data;
		if (!m || m.type !== 'calls') return;
		if (!result || m.stoppedMs !== result.StoppedMs) return;
		if (m.error) {
			$('tlSide').innerHTML = `<div class="bad">${esc(m.error)}</div>`;
			requested = undefined;
			return;
		}
		readTheme();
		data = prepare(m.result || {});
		st.v0 = 0;
		st.v1 = data.end;
		clampView(0, data.end);
		renderLegend();
		redraw();
	});

	readTheme();
})();
