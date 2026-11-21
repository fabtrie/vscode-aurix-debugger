// Trace panel webview, reverse debugging: while the mode is on it owns the trace, so the setup is
// off (banner); the History tab lists the checkpoints and goes back to one; the Instructions tab
// shows the instructions of the whole history (or of the last action) and its row action
// restores the target ("Go here") instead of the view-only replay ("Show in debugger").
(function () {
	const vscode = window.traceVscode;
	const $ = (id) => document.getElementById(id);
	let on = false;
	let history = [];

	function esc(s) {
		return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
	}

	function hex(n) {
		return typeof n === 'number' ? '0x' + n.toString(16).toUpperCase().padStart(8, '0') : '';
	}

	function setControls() {
		window.aurixReverse = on;
		$('reverseBanner').hidden = !on;
		document.querySelectorAll('#form input, #form select, #arm, #stopBtn, #auto, #autoSteps, #save, #saved').forEach((el) => {
			if (on) {
				el.dataset.reverseOff = el.disabled ? '1' : '';
				el.disabled = true;
			} else if (el.dataset.reverseOff !== undefined) {
				el.disabled = el.dataset.reverseOff === '1';
				delete el.dataset.reverseOff;
			}
		});
		$('form').classList.toggle('off', on);
		const tab = document.querySelector('#tabs .tab[data-tab="history"]');
		tab.hidden = !on;
		if (on) {
			$('tabs').hidden = false;
		}
		const b = $('insnReplay');
		b.textContent = on ? 'Go here' : 'Show in debugger';
		b.title = on
			? 'Restore the target to the point before the selected instruction: the action\'s checkpoint is written back and the core runs there again (registers, RAM; not peripherals)'
			: 'Show the selected instruction in the debugger: source, registers, locals, call stack (Step Back and Step move on from there; view only)';
	}

	function render() {
		const el = $('history');
		if (!on) {
			el.innerHTML = '';
			return;
		}
		if (!history.length) {
			el.innerHTML = '<div class="hint">No checkpoints yet: every step, run and write of the core saves one.</div>';
			return;
		}
		const rows = history
			.slice()
			.reverse()
			.map((c) => {
				const at = c.At || {};
				const file = at.File ? String(at.File).split(/[\\/]/).pop() + ':' + at.Line : '';
				const where = at.Function ? `${esc(at.Function)} <span class="hint">${esc(file)}</span>` : esc(hex(c.Pc));
				const cur = c.Current ? ' cur' : '';
				const go = c.Current ? '<span class="hint">the core is here</span>' : `<button type="button" class="secondary go" data-index="${c.Index}">Go here</button>`;
				const rec = c.Recorded ? (c.FromStart ? 'recorded' : 'recorded (end only)') : '';
				return `<tr class="hist${cur}"><td class="num">${c.Index}</td><td>${esc(c.Action || '(next action)')}</td><td>${where}</td><td class="hint">${esc(rec)}</td><td class="num hint">${c.CostMs ?? ''} ms</td><td>${go}</td></tr>`;
			})
			.join('');
		el.innerHTML =
			'<div class="hint">Newest first. Each row is the state before an action; "Go here" writes it back into the core (registers and RAM; peripherals, timers and other cores are not rewound).</div>' +
			`<table class="histTable"><thead><tr><th>#</th><th>Action</th><th>Before it at</th><th>Trace</th><th>Saved in</th><th></th></tr></thead><tbody>${rows}</tbody></table>`;
	}

	$('history').addEventListener('click', (e) => {
		const b = e.target.closest && e.target.closest('button.go');
		if (b) {
			vscode.postMessage({ type: 'reverseGoto', index: Number(b.dataset.index), row: 0 });
		}
	});

	window.addEventListener('message', (e) => {
		const m = e.data;
		if (m.type === 'reverse') {
			on = !!(m.status && m.status.Enabled);
			history = (m.history && m.history.Checkpoints) || [];
			setControls();
			render();
			window.dispatchEvent(new Event('reverse-changed'));
		}
	});
})();
