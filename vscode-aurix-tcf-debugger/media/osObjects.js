// @ts-check
/*
 * Renders the AUTOSAR OS object tables.
 *
 * The panel re-sends the whole table on every refresh (about once a second), so the work here is
 * to keep that from feeling like a reload: the scroll position, the filter text, the sort and the
 * focused row all survive, and only the cells whose text changed are touched.
 */
(function () {
	const vscode = acquireVsCodeApi();

	const tabsEl = /** @type {HTMLElement} */ (document.getElementById('tabs'));
	const gridEl = /** @type {HTMLElement} */ (document.getElementById('grid'));
	const filterEl = /** @type {HTMLInputElement} */ (document.getElementById('filter'));
	const countEl = /** @type {HTMLElement} */ (document.getElementById('count'));
	const readsEl = /** @type {HTMLElement} */ (document.getElementById('reads'));
	const messageEl = /** @type {HTMLElement} */ (document.getElementById('message'));
	const refreshEl = /** @type {HTMLElement} */ (document.getElementById('refresh'));
	const pauseEl = /** @type {HTMLButtonElement} */ (document.getElementById('pause'));
	const measureEl = /** @type {HTMLButtonElement} */ (document.getElementById('measure'));

	/** @type {any} */
	let table = null;
	/** Column index to sort by; -1 is the file order, which is what the generator chose. */
	let sortCol = -1;
	let sortDesc = false;
	let highlight = '';
	/** Signature of the DOM currently built, so a refresh can reuse it. */
	let builtFor = '';

	filterEl.addEventListener('input', () => render());
	refreshEl.addEventListener('click', () => vscode.postMessage({ type: 'refresh' }));
	pauseEl.addEventListener('click', () => vscode.postMessage({ type: 'pause' }));
	measureEl.addEventListener('click', () => {
		// Reading every untouched stack takes a moment, so say so rather than looking frozen.
		measureEl.disabled = true;
		measureEl.textContent = 'Measuring…';
		vscode.postMessage({ type: 'measure' });
	});

	window.addEventListener('message', e => {
		const m = e.data;
		if (m && m.type === 'table') {
			table = m.table;
			if (typeof m.highlight === 'string') {
				highlight = m.highlight;
			}
			renderTabs();
			render();
		}
	});

	function renderTabs() {
		const tabs = (table && table.tabs) || [];
		const sig = tabs.map(t => t.kind + ':' + t.count + ':' + t.active).join('|');
		if (tabsEl.dataset.sig === sig) {
			return;
		}
		tabsEl.dataset.sig = sig;
		tabsEl.textContent = '';
		for (const t of tabs) {
			const b = document.createElement('button');
			b.type = 'button';
			b.setAttribute('role', 'tab');
			b.setAttribute('aria-selected', String(!!t.active));
			b.appendChild(document.createTextNode(t.kind));
			const n = document.createElement('span');
			n.className = 'n';
			n.textContent = String(t.count);
			b.appendChild(n);
			b.addEventListener('click', () => vscode.postMessage({ type: 'select', kind: t.kind }));
			tabsEl.appendChild(b);
		}
	}

	/** Rows matching the filter, which tries the name and every cell. */
	function filtered() {
		const q = filterEl.value.trim().toLowerCase();
		const rows = (table && table.rows) || [];
		if (!q) {
			return rows.slice();
		}
		return rows.filter(r => r.name.toLowerCase().includes(q) || r.cells.some(c => c.text.toLowerCase().includes(q)));
	}

	function sorted(rows) {
		if (sortCol < 0) {
			return rows;
		}
		const key = r => (sortCol === 0 ? r.name : (r.cells[sortCol - 1] || { text: '' }).text);
		// Numeric where both sides are numbers, so priority 100 sorts below 420.
		return rows.slice().sort((a, b) => {
			const x = key(a);
			const y = key(b);
			const nx = Number(x);
			const ny = Number(y);
			const c = Number.isFinite(nx) && Number.isFinite(ny) && x !== '' && y !== '' ? nx - ny : x.localeCompare(y);
			return sortDesc ? -c : c;
		});
	}

	function render() {
		if (!table) {
			return;
		}
		messageEl.textContent = table.error || '';
		measureEl.hidden = !table.measurable;
		measureEl.disabled = false;
		measureEl.textContent = 'Measure coverage';
		// Pausing means "only on halt", so say which state the table is in rather than just stopping.
		pauseEl.textContent = table.paused ? 'Resume' : 'Pause';
		pauseEl.title = table.paused
			? 'Updating only when a core halts. Click to resume the periodic refresh.'
			: 'Update only when a core halts';
		const rate = table.paused ? 'on halt only' : (table.intervalMs || 0) + ' ms';
		readsEl.textContent = (table.reads ? table.reads + ' read' + (table.reads === 1 ? '' : 's') + ' · ' : '') + rate;

		const cols = table.columns || [];
		const rows = sorted(filtered());
		countEl.textContent = table.rows ? rows.length + ' of ' + table.rows.length : '';

		// Rebuild only when the shape changed; otherwise update the cells in place so the scroll
		// position and the selection survive a refresh.
		const sig = table.kind + '|' + cols.map(c => c.name).join(',') + '|' + rows.map(r => r.name).join(',');
		if (sig !== builtFor) {
			build(cols, rows);
			builtFor = sig;
		} else {
			update(rows);
		}
		markSort();
	}

	function build(cols, rows) {
		gridEl.textContent = '';
		if (!cols.length) {
			return;
		}
		const t = document.createElement('table');

		const thead = document.createElement('thead');
		const htr = document.createElement('tr');
		htr.appendChild(headerCell('Name', 0));
		cols.forEach((c, i) => htr.appendChild(headerCell(c.header, i + 1)));
		thead.appendChild(htr);
		t.appendChild(thead);

		const tbody = document.createElement('tbody');
		for (const r of rows) {
			const tr = document.createElement('tr');
			tr.dataset.name = r.name;
			if (r.name === highlight) {
				tr.className = 'hl';
			}
			const nameTd = document.createElement('td');
			nameTd.className = 'name';
			nameTd.textContent = r.name;
			tr.appendChild(nameTd);
			for (const cell of r.cells) {
				tr.appendChild(bodyCell(cell));
			}
			tbody.appendChild(tr);
		}
		t.appendChild(tbody);
		gridEl.appendChild(t);
		const hl = gridEl.querySelector('tr.hl');
		if (hl) {
			hl.scrollIntoView({ block: 'center' });
		}
	}

	function headerCell(text, index) {
		const th = document.createElement('th');
		th.textContent = text;
		th.addEventListener('click', () => {
			if (sortCol === index) {
				sortDesc = !sortDesc;
			} else {
				sortCol = index;
				sortDesc = false;
			}
			builtFor = '';
			render();
		});
		return th;
	}

	function bodyCell(cell) {
		const td = document.createElement('td');
		td.className = 'mono' + (cell.muted ? ' muted' : '') + (cell.error ? ' err' : '');
		if (cell.error) {
			td.title = cell.error;
		}
		setCell(td, cell);
		return td;
	}

	function setCell(td, cell) {
		td.textContent = '';
		if (typeof cell.pct === 'number') {
			// A bar and the number together: the colour is a second encoding, never the only one.
			const bar = document.createElement('span');
			bar.className = 'bar';
			const fill = document.createElement('span');
			fill.className = 'fill ' + (cell.pct >= 90 ? 'crit' : cell.pct >= 75 ? 'warn' : 'ok');
			fill.style.width = Math.max(0, Math.min(100, cell.pct)) + '%';
			bar.appendChild(fill);
			td.appendChild(bar);
			const n = document.createElement('span');
			n.className = 'pct';
			n.textContent = cell.text;
			td.appendChild(n);
			return;
		}
		if (cell.link) {
			const a = document.createElement('a');
			a.href = '#';
			a.textContent = cell.text;
			a.addEventListener('click', ev => {
				ev.preventDefault();
				vscode.postMessage({ type: 'follow', link: cell.link });
			});
			td.appendChild(a);
		} else {
			td.textContent = cell.text;
		}
	}

	function update(rows) {
		const trs = gridEl.querySelectorAll('tbody tr');
		rows.forEach((r, i) => {
			const tr = trs[i];
			if (!tr) {
				return;
			}
			const tds = tr.children;
			r.cells.forEach((cell, c) => {
				const td = /** @type {HTMLElement} */ (tds[c + 1]);
				// A bar cell is always redrawn: its width is not in the text.
				if (!td || (td.textContent === cell.text && typeof cell.pct !== 'number')) {
					return;
				}
				td.className = 'mono' + (cell.muted ? ' muted' : '') + (cell.error ? ' err' : '');
				td.title = cell.error || '';
				setCell(td, cell);
			});
		});
	}

	function markSort() {
		const ths = gridEl.querySelectorAll('thead th');
		ths.forEach((th, i) => {
			const old = th.querySelector('.dir');
			if (old) {
				old.remove();
			}
			if (i === sortCol) {
				const s = document.createElement('span');
				s.className = 'dir';
				s.textContent = sortDesc ? '▾' : '▴';
				th.appendChild(s);
			}
		});
	}
})();
