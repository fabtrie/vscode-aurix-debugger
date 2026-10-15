// @ts-check
/*
 * Draws the OS Profile: a lane of task blocks per core, the share of the window per task, and the
 * non-idle load per core.
 *
 * Colour follows the task, never its rank, so changing the window or the sort never repaints a
 * task that is still on screen. Past the fifth task everything folds into one neutral "other"
 * slot rather than inventing hues nobody can tell apart.
 */
(function () {
	const vscode = acquireVsCodeApi();

	const runEl = /** @type {HTMLButtonElement} */ (document.getElementById('run'));
	const periodEl = /** @type {HTMLSelectElement} */ (document.getElementById('period'));
	const windowEl = /** @type {HTMLSelectElement} */ (document.getElementById('window'));
	const clearEl = /** @type {HTMLElement} */ (document.getElementById('clear'));
	const statsEl = /** @type {HTMLElement} */ (document.getElementById('stats'));
	const messageEl = /** @type {HTMLElement} */ (document.getElementById('message'));
	const lanesEl = /** @type {HTMLElement} */ (document.getElementById('lanes'));
	const axisEl = /** @type {HTMLElement} */ (document.getElementById('axis'));
	const legendEl = /** @type {HTMLElement} */ (document.getElementById('legend'));
	const sharesEl = /** @type {HTMLElement} */ (document.getElementById('shares'));
	const loadEl = /** @type {HTMLElement} */ (document.getElementById('load'));

	/** Hues available before everything else folds into "other". */
	const SLOTS = 5;
	const IDLE = /^(INVALID_TASK|IdleTask|.*_?Idle(Task)?(_.*)?)$/i;

	/** @type {any} */
	let view = null;
	/** Colour slot per task, from the panel: assigned in first-seen order. */
	let colorOf = new Map();

	runEl.addEventListener("click", () => vscode.postMessage({ type: "pause" }));
	periodEl.addEventListener("change", () => vscode.postMessage({ type: "period", periodUs: Number(periodEl.value) }));
	windowEl.addEventListener('change', () => vscode.postMessage({ type: 'window', windowUs: Number(windowEl.value) }));
	clearEl.addEventListener('click', () => vscode.postMessage({ type: 'clear' }));

	window.addEventListener('message', e => {
		if (e.data && e.data.type === 'profile') {
			view = e.data.view;
			colorOf = new Map((view.colors || []).map(c => [c.task, c.index]));
			render();
		}
	});

	/** The CSS class carrying a task's colour. */
	function cls(task) {
		if (IDLE.test(task)) {
			return 'idle';
		}
		const i = colorOf.get(task);
		return i === undefined || i >= SLOTS ? 'cx' : 'c' + i;
	}

	function us(v) {
		if (v >= 1000000) {
			return (v / 1000000).toFixed(v >= 10000000 ? 0 : 1) + ' s';
		}
		if (v >= 1000) {
			return (v / 1000).toFixed(v >= 10000 ? 0 : 1) + ' ms';
		}
		return Math.round(v) + ' µs';
	}

	function render() {
		if (!view) {
			return;
		}
		// Pausing stops the sampler, not only the drawing: nothing is read while it is paused.
		runEl.textContent = view.paused ? "Resume" : "Pause";
		runEl.title = view.paused
			? "Not sampling. Click to start reading RUNNINGTASK again."
			: "Stop sampling the target";
		messageEl.textContent = view.message || '';
		periodEl.value = String(view.periodUs);
		windowEl.value = String(view.windowUs);

		const bits = [];
		bits.push(view.samples + ' switch' + (view.samples === 1 ? '' : 'es'));
		bits.push(view.paused ? "not sampling" : "every " + us(view.periodUs));
		bits.push("redrawn every " + view.pollMs + " ms");
		if (view.dropped) {
			bits.push(view.dropped + ' dropped');
		}
		if (view.missed) {
			bits.push(view.missed + ' missed ticks');
		}
		statsEl.textContent = bits.join(' · ');

		renderLanes();
		renderAxis();
		renderLegend();
		renderShares();
		renderLoad();
	}

	function renderLanes() {
		lanesEl.textContent = '';
		if (!view.lanes.length) {
			const p = document.createElement('div');
			p.className = 'note';
			p.style.padding = '0 14px';
			p.textContent = view.paused ? "Paused: the target is not being sampled." : view.running ? "Waiting for the first task switch…" : "Not sampling.";
			lanesEl.appendChild(p);
			return;
		}
		for (const lane of view.lanes) {
			const row = document.createElement('div');
			row.className = 'lane';
			const name = document.createElement('span');
			name.className = 'name';
			name.textContent = 'cpu' + lane.core;
			row.appendChild(name);
			const track = document.createElement('div');
			track.className = 'track';
			for (const b of lane.blocks) {
				const el = document.createElement('div');
				el.className = 'blk ' + cls(b.task);
				el.style.left = (b.t / view.windowUs) * 100 + '%';
				el.style.width = Math.max(0, (b.us / view.windowUs) * 100) + '%';
				el.title = b.task + ' · ' + us(b.us);
				// Only label a block wide enough to read; the legend covers the rest.
				if (b.us / view.windowUs > 0.1 && !IDLE.test(b.task)) {
					el.textContent = b.task;
				}
				track.appendChild(el);
			}
			row.appendChild(track);
			lanesEl.appendChild(row);
		}
	}

	function renderAxis() {
		axisEl.textContent = '';
		const w = view.windowUs;
		for (let i = 4; i >= 0; i--) {
			const s = document.createElement('span');
			s.textContent = i === 0 ? 'now' : '−' + us((w * i) / 4);
			axisEl.appendChild(s);
		}
	}

	function renderLegend() {
		legendEl.textContent = '';
		const seen = new Set();
		for (const lane of view.lanes) {
			for (const b of lane.blocks) {
				seen.add(b.task);
			}
		}
		// A legend is always present once there is more than one thing to tell apart.
		for (const task of [...seen].sort()) {
			const item = document.createElement('span');
			item.className = 'item';
			const sw = document.createElement('span');
			sw.className = 'sw';
			sw.style.background = swatch(task);
			item.appendChild(sw);
			item.appendChild(document.createTextNode(IDLE.test(task) ? task + ' (idle)' : task));
			legendEl.appendChild(item);
		}
	}

	function swatch(task) {
		const c = cls(task);
		return c === 'idle' ? 'var(--task-idle)' : c === 'cx' ? 'var(--task-other)' : 'var(--task-' + c.slice(1) + ')';
	}

	function renderShares() {
		sharesEl.textContent = '';
		let core = -1;
		for (const s of view.shares) {
			if (s.core !== core) {
				core = s.core;
				const h = document.createElement('div');
				h.className = 'share head';
				const n = document.createElement('span');
				n.className = 'n';
				n.textContent = 'CORE ' + core;
				h.appendChild(n);
				h.appendChild(document.createElement('span'));
				h.appendChild(document.createElement('span'));
				sharesEl.appendChild(h);
			}
			const row = document.createElement('div');
			row.className = 'share';
			const n = document.createElement('span');
			n.className = 'n';
			n.textContent = s.task;
			n.title = s.task + ' · ' + us(s.us);
			row.appendChild(n);
			const track = document.createElement('div');
			track.className = 'track';
			const fill = document.createElement('div');
			fill.className = 'fill';
			fill.style.width = Math.min(100, s.percent) + '%';
			fill.style.background = swatch(s.task);
			track.appendChild(fill);
			row.appendChild(track);
			const v = document.createElement('span');
			v.className = 'v';
			v.textContent = s.percent.toFixed(1) + '%';
			row.appendChild(v);
			sharesEl.appendChild(row);
		}
	}

	function renderLoad() {
		loadEl.textContent = '';
		for (const l of view.load) {
			const g = document.createElement('div');
			g.className = 'gauge';
			const big = document.createElement('div');
			const n = document.createElement('span');
			n.className = 'big';
			n.textContent = String(Math.round(l.percent));
			big.appendChild(n);
			const p = document.createElement('span');
			p.className = 'pct';
			p.textContent = '%';
			big.appendChild(p);
			g.appendChild(big);
			const meter = document.createElement('div');
			meter.className = 'meter';
			const fill = document.createElement('span');
			fill.style.width = Math.min(100, l.percent) + '%';
			meter.appendChild(fill);
			g.appendChild(meter);
			const who = document.createElement('div');
			who.className = 'who';
			who.textContent = 'cpu' + l.core;
			g.appendChild(who);
			loadEl.appendChild(g);
		}
	}
})();
