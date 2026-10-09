// @ts-check
// Live Watch webview: renders the rows sent by LiveWatchInstance (src/liveWatchView.ts), in the
// sidebar view or in a panel.
// Rows are kept by id and only changed parts of the DOM are touched, so an update does not
// re-create rows (no flickering tooltips or buttons).
(function () {
	// @ts-ignore acquireVsCodeApi is provided by the webview host
	const vscode = acquireVsCodeApi();
	const tree = /** @type {HTMLElement} */ (document.getElementById('tree'));
	const empty = /** @type {HTMLElement} */ (document.getElementById('empty'));

	const ICONS = {
		chevron: '<svg viewBox="0 0 16 16"><path d="M5.7 13.7L5 13l5-5-5-5 .7-.7L11.4 8z"/></svg>',
		edit: '<svg viewBox="0 0 16 16"><path d="M11.5 1.5l3 3L6 13H3v-3l8.5-8.5zm0 1.4L4 10.4V12h1.6l7.5-7.5-1.6-1.6z"/></svg>',
		pause: '<svg viewBox="0 0 16 16"><path d="M4 3h3v10H4zm5 0h3v10H9z"/></svg>',
		resume: '<svg viewBox="0 0 16 16"><path d="M4 2.5v11l9-5.5z"/></svg>',
		remove: '<svg viewBox="0 0 16 16"><path d="M3.7 3L3 3.7 7.3 8 3 12.3l.7.7L8 8.7l4.3 4.3.7-.7L8.7 8 13 3.7l-.7-.7L8 7.3z"/></svg>',
	};
	const ACTION_TITLES = { edit: 'Set Value', pause: 'Pause Updates', resume: 'Resume Updates', remove: 'Remove Expression' };

	/**
	 * @typedef {{ id: string, depth: number, kind: 'group'|'root'|'child'|'more', name: string, value: string,
	 *   valueKind: string, hasChildren: boolean, expanded: boolean, paused: boolean, readOnly: boolean, renamable: boolean,
	 *   tooltip: string, context: string }} Row
	 * @typedef {{ el: HTMLElement, row: Row | undefined, actions: string }} Entry
	 */

	/** @type {Map<string, Entry>} */
	const entries = new Map();
	/** @type {Row[]} */
	let rows = [];
	// The instance id is kept in the state so that VS Code can restore a panel after a reload.
	const instanceId = document.body.dataset.instanceId;
	/** @type {string | undefined} */
	let selectedId = (vscode.getState() || {}).selectedId;
	vscode.setState({ instanceId, selectedId });
	/** @type {{ id: string, input: HTMLInputElement, valueEl: HTMLElement } | undefined} */
	let editing;

	window.addEventListener('message', (event) => {
		const message = event.data;
		if (message?.type === 'rows') {
			setRows(message.rows || []);
			empty.hidden = !message.emptyMessage;
			empty.textContent = message.emptyMessage || '';
		} else if (message?.type === 'edit') {
			startEdit(message.id, message.mode === 'expression' ? 'expression' : 'value');
		} else if (message?.type === 'select') {
			select(message.id);
		}
	});

	/** @param {Row[]} newRows */
	function setRows(newRows) {
		const seen = new Set();
		/** @type {ChildNode | null} */
		let next = tree.firstChild;
		for (const row of newRows) {
			let entry = entries.get(row.id);
			if (!entry) {
				entry = { el: createRow(row.id), row: undefined, actions: '' };
				entries.set(row.id, entry);
			}
			updateRow(entry, row);
			if (entry.el !== next) {
				tree.insertBefore(entry.el, next);
			} else {
				next = next.nextSibling;
			}
			seen.add(row.id);
		}
		for (const [id, entry] of entries) {
			if (!seen.has(id)) {
				entry.el.remove();
				entries.delete(id);
			}
		}
		rows = newRows;
		if (editing && !entries.has(editing.id)) {
			editing = undefined;
		}
		if (selectedId !== undefined && !entries.has(selectedId)) {
			select(undefined);
		}
	}

	/** @param {string} id */
	function createRow(id) {
		const el = document.createElement('div');
		el.className = 'row';
		el.setAttribute('role', 'treeitem');
		el.dataset.id = id;
		el.innerHTML =
			`<div class="twistie">${ICONS.chevron}</div>` +
			'<div class="contents"><div class="expression"></div></div>' +
			'<div class="actions"></div>';
		if (id === selectedId) {
			el.classList.add('selected');
		}
		return el;
	}

	/**
	 * @param {Entry} entry
	 * @param {Row} row
	 */
	function updateRow(entry, row) {
		const old = entry.row;
		const el = entry.el;
		entry.row = row;
		if (old?.depth !== row.depth) {
			el.style.paddingLeft = `${row.depth * 8}px`;
			el.setAttribute('aria-level', String(row.depth + 1));
		}
		if (old?.hasChildren !== row.hasChildren || old?.expanded !== row.expanded) {
			const twistie = /** @type {HTMLElement} */ (el.querySelector('.twistie'));
			twistie.classList.toggle('collapsible', row.hasChildren);
			twistie.classList.toggle('expanded', row.expanded);
			if (row.hasChildren) {
				el.setAttribute('aria-expanded', String(row.expanded));
			} else {
				el.removeAttribute('aria-expanded');
			}
		}
		if (old?.tooltip !== row.tooltip) {
			el.title = row.tooltip;
		}
		if (old?.context !== row.context) {
			el.dataset.vscodeContext = row.context;
		}

		const expression = /** @type {HTMLElement} */ (el.querySelector('.contents > div'));
		if (old?.kind !== row.kind) {
			if (row.kind === 'group') {
				expression.className = 'group';
				expression.innerHTML = '<span class="group-label"></span><span class="description"></span>';
			} else {
				expression.className = 'expression';
				expression.innerHTML = '<span class="name"><span class="monaco-highlighted-label"></span></span><span class="value"></span>';
			}
		}
		if (row.kind === 'group') {
			setText(expression.querySelector('.group-label'), row.name);
			setText(expression.querySelector('.description'), row.value);
		} else {
			setText(expression.querySelector('.monaco-highlighted-label'), row.kind === 'more' ? row.name : `${row.name}:`);
			if (editing?.id !== row.id) {
				const valueEl = /** @type {HTMLElement} */ (expression.querySelector('.value'));
				if (old?.kind !== row.kind || old?.valueKind !== row.valueKind) {
					setValueKind(valueEl, row.valueKind);
				}
				if (valueEl.textContent !== row.value) {
					valueEl.textContent = row.value;
					if (old && old.kind === row.kind && row.valueKind !== 'error') {
						flash(valueEl);
					}
				}
			}
		}

		const actions = actionsFor(row);
		if (entry.actions !== actions) {
			entry.actions = actions;
			const bar = /** @type {HTMLElement} */ (el.querySelector('.actions'));
			bar.textContent = '';
			for (const action of actions ? actions.split(',') : []) {
				const button = document.createElement('button');
				button.className = 'action';
				button.dataset.action = action;
				button.title = ACTION_TITLES[/** @type {keyof ACTION_TITLES} */ (action)];
				button.tabIndex = -1;
				button.innerHTML = ICONS[/** @type {keyof ICONS} */ (action)];
				bar.appendChild(button);
			}
		}
	}

	/** @param {Row} row */
	function actionsFor(row) {
		switch (row.kind) {
			case 'root':
				return `${row.readOnly ? '' : 'edit,'}${row.paused ? 'resume' : 'pause'},remove`;
			case 'child':
				return row.hasChildren ? '' : 'edit';
			default:
				return '';
		}
	}

	/**
	 * @param {Element | null} el
	 * @param {string} text
	 */
	function setText(el, text) {
		if (el && el.textContent !== text) {
			el.textContent = text;
		}
	}

	/**
	 * Sets the value's type class (number, string, ...) without touching the changed highlight.
	 * @param {HTMLElement} el
	 * @param {string} kind
	 */
	function setValueKind(el, kind) {
		el.classList.remove('number', 'string', 'boolean', 'error', 'other');
		el.classList.add('value', kind);
	}

	/** How long a changed value stays highlighted after its last change, before it fades out. */
	const CHANGED_HOLD_MS = 3000;
	/** @type {WeakMap<HTMLElement, ReturnType<typeof setTimeout>>} */
	const changedTimers = new WeakMap();

	/**
	 * Highlights a changed value. Further changes keep the highlight steady (no restart, so no
	 * flicker) and only postpone the fade-out.
	 * @param {HTMLElement} el
	 */
	function flash(el) {
		el.classList.add('changed');
		clearTimeout(changedTimers.get(el));
		changedTimers.set(el, setTimeout(() => {
			changedTimers.delete(el);
			el.classList.remove('changed');
		}, CHANGED_HOLD_MS));
	}

	/** @param {string | undefined} id */
	function select(id) {
		if (selectedId === id) {
			return;
		}
		entries.get(selectedId ?? '')?.el.classList.remove('selected');
		selectedId = id;
		const entry = id !== undefined ? entries.get(id) : undefined;
		entry?.el.classList.add('selected');
		entry?.el.scrollIntoView({ block: 'nearest' });
		vscode.setState({ instanceId, selectedId });
		vscode.postMessage({ type: 'select', id });
	}

	/** @param {string} id */
	function toggle(id) {
		vscode.postMessage({ type: 'toggle', id });
	}

	/**
	 * @param {Row | undefined} row
	 * @param {'value' | 'expression'} mode
	 */
	function isEditable(row, mode) {
		if (mode === 'expression') {
			return row?.kind === 'root' && row.renamable;
		}
		return !!row && (row.kind === 'root' || row.kind === 'child') && !row.hasChildren && !row.readOnly;
	}

	/**
	 * Edits a value in place, or the expression of a watched entry (the name and value are replaced
	 * by one input, like "Edit Expression" in the Watch view).
	 * @param {string} id
	 * @param {'value' | 'expression'} mode
	 */
	function startEdit(id, mode) {
		const entry = entries.get(id);
		if (!entry || !entry.row || !isEditable(entry.row, mode) || editing) {
			return;
		}
		select(id);
		const row = entry.row;
		const nameEl = /** @type {HTMLElement} */ (entry.el.querySelector('.name'));
		const valueEl = /** @type {HTMLElement} */ (entry.el.querySelector('.value'));
		const input = document.createElement('input');
		input.className = mode === 'expression' ? 'edit expression-edit' : 'edit';
		const initial = mode === 'expression' ? row.name : row.valueKind === 'error' ? '' : row.value;
		input.value = initial;
		if (mode === 'expression') {
			nameEl.hidden = true;
		}
		valueEl.hidden = true;
		valueEl.after(input);
		editing = { id, input, valueEl };
		let done = false;
		/** @param {boolean} commit */
		const finish = (commit) => {
			if (done) {
				return;
			}
			done = true;
			const text = input.value.trim();
			const changed = commit && text !== '' && text !== initial.trim();
			input.remove();
			editing = undefined;
			// Updates received while editing were not applied to the value.
			const current = entries.get(id)?.row;
			if (current) {
				setValueKind(valueEl, current.valueKind);
				valueEl.textContent = current.value;
			}
			nameEl.hidden = false;
			valueEl.hidden = false;
			if (changed) {
				vscode.postMessage(mode === 'expression'
					? { type: 'rename', id, expression: text }
					: { type: 'setValue', id, value: input.value });
			}
			tree.focus();
		};
		input.addEventListener('keydown', (event) => {
			event.stopPropagation();
			if (event.key === 'Enter') {
				finish(true);
			} else if (event.key === 'Escape') {
				finish(false);
			}
		});
		input.addEventListener('blur', () => finish(true));
		input.addEventListener('click', (event) => event.stopPropagation());
		input.addEventListener('dblclick', (event) => event.stopPropagation());
		input.focus();
		input.select();
	}

	/** @param {EventTarget | null} target */
	function rowOf(target) {
		const el = target instanceof Element ? target.closest('.row') : null;
		return el instanceof HTMLElement ? el.dataset.id : undefined;
	}

	tree.addEventListener('click', (event) => {
		const id = rowOf(event.target);
		if (id === undefined) {
			return;
		}
		const row = entries.get(id)?.row;
		const button = event.target instanceof Element ? event.target.closest('.action') : null;
		if (button instanceof HTMLElement) {
			const action = button.dataset.action;
			if (action === 'edit') {
				startEdit(id, 'value');
			} else {
				vscode.postMessage({ type: action, id });
			}
			return;
		}
		select(id);
		if (row?.hasChildren) {
			toggle(id);
		}
	});

	tree.addEventListener('dblclick', (event) => {
		const id = rowOf(event.target);
		if (id === undefined || (event.target instanceof Element && event.target.closest('.action'))) {
			return;
		}
		const row = entries.get(id)?.row;
		// Double-click on the name (or on a struct, whose value cannot be set) edits the expression.
		const onName = event.target instanceof Element && event.target.closest('.name');
		startEdit(id, row?.kind === 'root' && row.renamable && (onName || !isEditable(row, 'value')) ? 'expression' : 'value');
	});

	tree.addEventListener('contextmenu', (event) => {
		const id = rowOf(event.target);
		if (id !== undefined) {
			select(id);
		}
	});

	tree.addEventListener('keydown', (event) => {
		const index = rows.findIndex((r) => r.id === selectedId);
		const row = index >= 0 ? rows[index] : undefined;
		switch (event.key) {
			case 'ArrowDown':
				select(rows[Math.min(rows.length - 1, index + 1)]?.id);
				break;
			case 'ArrowUp':
				select(rows[Math.max(0, index - 1)]?.id);
				break;
			case 'Home':
				select(rows[0]?.id);
				break;
			case 'End':
				select(rows[rows.length - 1]?.id);
				break;
			case 'ArrowRight':
				if (row?.hasChildren && !row.expanded) {
					toggle(row.id);
				} else if (row?.expanded && rows[index + 1]) {
					select(rows[index + 1].id);
				}
				break;
			case 'ArrowLeft':
				if (row?.hasChildren && row.expanded) {
					toggle(row.id);
				} else if (row) {
					const parent = rows.slice(0, index).reverse().find((r) => r.depth < row.depth);
					select(parent?.id);
				}
				break;
			case ' ':
				if (row?.hasChildren) {
					toggle(row.id);
				}
				break;
			case 'Enter':
				if (row) {
					startEdit(row.id, 'value');
				}
				break;
			case 'F2':
				// Like the Watch view: F2 edits the expression of a watched entry.
				if (row) {
					startEdit(row.id, row.kind === 'root' && row.renamable ? 'expression' : 'value');
				}
				break;
			case 'Delete':
				if (row?.kind === 'root') {
					vscode.postMessage({ type: 'remove', id: row.id });
				}
				break;
			default:
				return;
		}
		event.preventDefault();
	});

	// The Live Watch used last is the one "Add to Live Watch" offers first.
	window.addEventListener('focus', () => vscode.postMessage({ type: 'focus' }));

	vscode.postMessage({ type: 'ready' });
	if (selectedId !== undefined) {
		vscode.postMessage({ type: 'select', id: selectedId });
	}
})();
