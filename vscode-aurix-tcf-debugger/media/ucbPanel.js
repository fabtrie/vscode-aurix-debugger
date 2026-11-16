// UCBs panel (webview side). The data is the debug server's `ucb ... -json`; see ucbPanel.ts.
(function () {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const state = vscode.getState() || {};
  let view = null;
  let selected = state.selected || 'BMHD0';
  let fieldQuery = '';
  let fieldOnly = false;
  let review = null;
  let reviewSource = 'staged';

  // --- helpers -----------------------------------------------------------------------------
  const fieldsOf = (u) => u.words.flatMap((w) => w.fields.map((f) => ({ w, f })));
  const stagedCount = (u) => fieldsOf(u).filter(({ f }) => f.staged !== undefined).length;
  const totalStaged = () => (view ? view.ucbs.reduce((n, u) => n + stagedCount(u), 0) : 0);
  const stagedUcbs = () => (view ? view.ucbs.filter((u) => stagedCount(u) > 0).length : 0);
  const spec = (u, w, f) => (w.raw ? `${u.name}.${w.name}` : `${u.name}.${w.name}.${f.name}`);
  const hexw = (v, bits) => '0x' + (v >>> 0).toString(16).toUpperCase().padStart(Math.max(1, Math.ceil(bits / 4)), '0');
  const width = (f) => f.msb - f.lsb + 1;

  function optionText(f, v) {
    if (f.kind === 'bool') return v ? 'yes' : 'no';
    const o = (f.options || []).find((x) => x[0] === v);
    return o ? o[1] : null;
  }

  // --- list ----------------------------------------------------------------------------------
  function renderList() {
    if (!view) return;
    const q = $('ucbFilter').value.trim().toLowerCase();
    const only = $('ucbChanged').checked;
    let h = '';
    let group = null;
    for (const u of view.ucbs) {
      const changed = u.changed > 0;
      const st = stagedCount(u);
      const bad = u.orig_copy_differ || (u.messages || []).some((m) => m.level === 'warn');
      if (only && !changed && !st) continue;
      if (q && !u.name.toLowerCase().includes(q) && !fieldsOf(u).some(({ w, f }) => `${w.name} ${f.name} ${f.label}`.toLowerCase().includes(q))) continue;
      if (u.group !== group) {
        group = u.group;
        h += `<div class="grp">${esc(group)}</div>`;
      }
      const ifx = u.class === 'infineon';
      const dot = st ? 'staged' : bad ? 'bad' : changed ? 'changed' : ifx ? 'ifx' : '';
      const tag = st ? `${st} pending` : u.orig_copy_differ ? 'ORIG ≠ COPY' : changed ? `${u.changed} changed` : ifx ? 'Infineon' : u.state === 'CONFIRMED' ? 'confirmed' : 'default';
      h += `<div class="item ${u.name === selected ? 'sel' : ''} ${!changed && !st && !ifx ? 'default' : ''}" data-u="${esc(u.name)}">
        <span class="dot ${dot}"></span><span class="name">${esc(u.name)}</span><span class="tag">${esc(tag)}</span></div>`;
    }
    $('list').innerHTML = h || '<div class="grp">No UCB matches</div>';
    $('list').querySelectorAll('.item').forEach((el) =>
      el.addEventListener('click', () => {
        selected = el.dataset.u;
        fieldQuery = '';
        fieldOnly = false;
        vscode.setState({ selected });
        renderList();
        renderDetail();
      }),
    );
  }

  // --- detail ----------------------------------------------------------------------------------
  function control(u, w, f) {
    const key = spec(u, w, f);
    const disabled = !u.writable || !w.readable ? 'disabled' : '';
    const v = f.staged !== undefined ? f.staged : f.value;
    if (v === null || v === undefined) return `<span class="muted">${esc(f.text)}</span>`;
    if (f.kind === 'auto') return `${esc(f.staged_text ?? f.text)} <span class="muted">${esc(f.auto)}</span>`;
    if (f.kind === 'bool') return `<label><input type="checkbox" data-k="${esc(key)}" data-kind="bool" ${v ? 'checked' : ''} ${disabled}> ${v ? 'yes' : 'no'}</label>`;
    if (f.kind === 'enum' || f.kind === 'confirm') {
      const opts = (f.options || []).slice();
      if (!opts.some((o) => o[0] === v)) opts.push([v, hexw(v, width(f))]);
      return `<select data-k="${esc(key)}" data-kind="enum" ${disabled}>${opts.map((o) => `<option value="${o[0]}" ${o[0] === v ? 'selected' : ''}>${esc(o[1])}</option>`).join('')}</select>`;
    }
    const text = f.staged !== undefined ? f.staged_text : f.text;
    return `<input type="text" class="mono" data-k="${esc(key)}" data-kind="${f.kind}" value="${esc(text)}" ${f.kind === 'sectors' ? 'placeholder="none"' : ''} ${disabled}>`;
  }

  function rowClass(u, f) {
    if (f.staged !== undefined) return 'staged';
    if (f.changed && u.class !== 'infineon') return 'changed';
    return '';
  }

  function fieldMatches(u, w, f) {
    const q = fieldQuery.trim().toLowerCase();
    if (q && !`${w.name} ${f.name} ${f.label}`.toLowerCase().includes(q)) return false;
    if (fieldOnly && !(f.staged !== undefined || (f.changed && u.class !== 'infineon') || f.copy_differs)) return false;
    return true;
  }

  function undoLink(u, w, f) {
    // Computed fields (header ID, checksums) follow the others: nothing to undo on their own.
    if (f.staged === undefined || f.kind === 'auto') return '';
    const orig = f.kind === 'sectors' || f.kind === 'num' || f.kind === 'auto' ? f.text : String(f.value);
    return `<span class="undo" title="Undo" data-undo="${esc(spec(u, w, f))}" data-orig="${esc(orig)}">↺</span>`;
  }

  function fieldRow(u, w, f) {
    const was = f.staged !== undefined ? `<span class="was">(was ${esc(f.text)})</span>` : '';
    const copy = f.copy_differs ? `<span class="copy">COPY: ${esc(optionText(f, f.copy) ?? hexw(f.copy, width(f)))}</span>` : '';
    const def = (rowClass(u, f) || f.staged !== undefined) && f.default_text !== undefined ? `default: ${esc(f.default_text)}` : '';
    return `<div class="f ${rowClass(u, f)}"><div class="lbl">${esc(f.label)}<small>${f.kind === 'sectors' || f.name === f.label ? '' : esc(f.name)}</small></div>
      <div class="val">${control(u, w, f)}${undoLink(u, w, f)}${was}${copy}</div><div class="def">${def}</div></div>`;
  }

  function renderDetail() {
    const el = $('detail');
    if (!view) {
      el.innerHTML = '';
      return;
    }
    const u = view.ucbs.find((x) => x.name === selected) || view.ucbs[0];
    selected = u.name;
    const pair = u.copy !== null && u.copy !== undefined;
    let h = `<div class="dhead"><h2>UCB_${esc(u.name)}</h2><div class="line">
      <span class="pill">${esc(u.state)}</span>
      ${pair ? `<span class="pill ${u.orig_copy_differ ? 'bad' : 'ok'}">${u.orig_copy_differ ? 'ORIG ≠ COPY' : 'ORIG = COPY'}</span>` : ''}
      <span class="pill">${esc({ reversible: 'Can be undone', password: 'Undo needs the password once CONFIRMED', permanent: 'Permanent', read_only: 'Read only' }[u.risk] || u.risk)}</span>
      <span class="muted">${esc(u.addr.join(' / '))}</span></div>
      <div class="ffilter"><input type="search" id="fieldFilter" placeholder="Filter fields…" value="${esc(fieldQuery)}" autocomplete="off">
      <label class="muted"><input type="checkbox" id="fieldChanged" ${fieldOnly ? 'checked' : ''}> Only changed</label><span class="muted" id="fcount"></span></div></div>`;
    const filtering = fieldQuery.trim() || fieldOnly;
    if (!filtering) {
      h += `<div class="about">${esc(u.about)}</div>`;
      if (u.read_only_reason) h += `<div class="msg info">${esc(u.read_only_reason)}</div>`;
      for (const m of u.messages || []) h += `<div class="msg ${m.level === 'warn' ? '' : m.level === 'ok' ? 'ok' : 'info'}">${esc(m.text)}</div>`;
      if (u.reserved_nonzero && u.reserved_nonzero.length) {
        h += `<div class="msg">Reserved words not erased: ${u.reserved_nonzero.map((r) => `+${esc(r.off)} = ${esc(r.orig)}`).join(', ')}</div>`;
      }
    }
    let shown = 0;
    // Password: one row for PW0..PW7.
    const pwWords = u.words.filter((w) => /^PW[0-7]$/.test(w.name));
    if (pwWords.length && (!filtering || 'password pw'.includes(fieldQuery.trim().toLowerCase()) || (fieldOnly && pwWords.some((w) => w.fields[0].staged !== undefined)))) {
      const vals = pwWords.map((w) => w.fields[0]);
      const set = vals.some((f) => f.text === 'set' || (typeof f.value === 'number' && f.value !== 0));
      const unreadable = vals.some((f) => f.text === 'unreadable');
      const revealed = vals.every((f) => typeof f.value === 'number');
      const st = vals.some((f) => f.staged !== undefined);
      const txt = unreadable ? 'unreadable' : revealed ? vals.map((f) => hexw(f.value, 32)).join(' ') : set ? 'set' : 'none (all zero)';
      const known = u.password && u.password.known ? ' · the session knows it' : '';
      h += `<div class="word"><div class="wh"><b>Password</b><span class="muted mono">+0x100..+0x11C</span></div>
        <div class="f ${st ? 'staged' : ''}"><div class="lbl">PW0..PW7</div><div class="val mono">${esc(st ? 'new password (not written yet)' : txt)}<span class="muted">${esc(known)}</span></div>
        <div class="def">${!revealed && set ? '<button class="link" id="pwShow">Show</button> · ' : ''}${u.writable ? '<button class="link" id="pwChange">Change…</button>' : ''}</div></div></div>`;
      shown++;
    }
    // Raw words of unpublished content: a grid.
    const raw = u.words.filter((w) => w.raw);
    const named = u.words.filter((w) => !w.raw && !/^PW[0-7]$/.test(w.name));
    for (const w of named) {
      const fs = w.fields.filter((f) => fieldMatches(u, w, f));
      if (!fs.length) continue;
      shown += fs.length;
      const installed = w.installed ? ` <span class="muted" title="installed by the DMU at the last reset">installed ${esc(w.installed)}${w.pending_reset ? ' · active after reset' : ''}</span>` : '';
      h += `<div class="word"><div class="wh"><b>${esc(w.name)}</b><span class="muted mono">+${esc(w.off)}</span>${installed}<span class="raw mono">${esc(w.readable ? w.value ?? '' : 'unreadable')}</span></div>${fs.map((f) => fieldRow(u, w, f)).join('')}</div>`;
    }
    if (raw.length) {
      const rows = [];
      for (let i = 0; i < raw.length; i += 8) rows.push(raw.slice(i, i + 8));
      const vis = rows.filter((r) => r.some((w) => fieldMatches(u, w, w.fields[0])));
      if (vis.length && (!fieldQuery.trim() || 'raw content'.includes(fieldQuery.trim().toLowerCase()) || fieldOnly)) {
        shown += vis.length * 8;
        h += `<div class="word"><div class="wh"><b>Content (not published)</b><span class="muted">raw words${raw[0].secret ? ' · may hold HSM keys' : ''}</span></div><table class="t">
          ${vis.map((r) => `<tr><td class="muted mono">+${esc(r[0].off)}</td>${r.map((w) => `<td class="${rowClass(u, w.fields[0])}">${control(u, w, w.fields[0])}</td>`).join('')}</tr>`).join('')}</table></div>`;
      }
    }
    if (!shown && filtering) h += '<div class="none">No field matches the filter.</div>';
    el.innerHTML = h;
    $('fcount').textContent = filtering ? `${shown} shown` : '';
    const ff = $('fieldFilter');
    ff.addEventListener('input', () => {
      fieldQuery = ff.value;
      renderDetail();
      const n = $('fieldFilter');
      n.focus();
      n.setSelectionRange(fieldQuery.length, fieldQuery.length);
    });
    $('fieldChanged').addEventListener('change', (e) => {
      fieldOnly = e.target.checked;
      renderDetail();
    });
    el.querySelectorAll('[data-k]').forEach((c) =>
      c.addEventListener('change', () => {
        let value;
        if (c.dataset.kind === 'bool') value = c.checked ? 'yes' : 'no';
        else if (c.dataset.kind === 'enum') value = c.value;
        else value = c.value.trim() || (c.dataset.kind === 'sectors' ? 'none' : '0');
        vscode.postMessage({ type: 'set', field: c.dataset.k, value });
      }),
    );
    el.querySelectorAll('[data-undo]').forEach((x) => x.addEventListener('click', () => vscode.postMessage({ type: 'set', field: x.dataset.undo, value: x.dataset.orig })));
    const show = $('pwShow');
    if (show) show.addEventListener('click', () => vscode.postMessage({ type: 'reveal', ucb: u.name }));
    const change = $('pwChange');
    if (change) change.addEventListener('click', () => vscode.postMessage({ type: 'password', ucb: u.name }));
  }

  function renderStatus() {
    if (!view) {
      $('status').innerHTML = '';
      $('device').textContent = '';
      return;
    }
    const d = view.device;
    $('device').textContent = `${d.name} · UID ${d.uid || '-'} · read ${new Date(view.time * 1000).toLocaleTimeString()}`;
    const s = view.status;
    const card = (k, v, warn) => `<div class="card ${warn ? 'warn' : ''}"><div class="k">${esc(k)}</div><div class="v">${esc(v)}</div></div>`;
    $('status').innerHTML =
      card('Debug access', s.debug, s.debug !== 'open') +
      card('Flash read protection', s.flash_read_protection ? 'on' : 'off', s.flash_read_protection) +
      card('Boots from', s.boot, /no valid/.test(s.boot)) +
      card('HSM', s.hsm, false) +
      card('Permanent (OTP)', s.otp, !/nothing/.test(s.otp));
  }

  function renderBar() {
    const n = totalStaged();
    $('stagedText').innerHTML = n ? `<b>${n} change${n > 1 ? 's' : ''}</b> in ${stagedUcbs()} UCB${stagedUcbs() > 1 ? 's' : ''}, not written yet` : '<span class="muted">No changes</span>';
    $('flagText').textContent = view && !view.write_enabled ? 'Writing is off: set "ucbWrite": true in the launch configuration' : '';
    $('reviewBtn').disabled = !n;
    $('discard').disabled = !n;
  }

  // --- compare with HEX ----------------------------------------------------------------------
  let compare = null;
  let cmpOnlyDiff = true;
  const cmpUcb = (name) => compare.ucbs.find((u) => u.name === name);
  const isPw = (w) => /^PW[0-7]$/.test(w.name);

  function cmpStatus(u) {
    if (u.in_file === 'none') return ['absent', 'not in the file'];
    if (!u.differs) return ['same', 'same as the device'];
    if (u.read_only_reason) return ['ro', `${u.differs} differ · read only`];
    return [u.taken ? 'staged' : 'diff', u.taken === u.differs ? `${u.differs} taken` : u.taken ? `${u.taken} of ${u.differs} taken` : `${u.differs} differ`];
  }

  function renderCompareBanner() {
    const b = $('compareBanner');
    $('legend').hidden = !!compare;
    $('compareLegend').hidden = !compare;
    if (!compare) {
      b.hidden = true;
      return;
    }
    const open = compare.ucbs.filter((u) => u.differs && !u.read_only_reason && u.taken < u.differs).length;
    b.hidden = false;
    const names = compare.files.map((f) => f.split(/[\\/]/).pop());
    b.innerHTML = `Comparing with <b class="mono" title="${esc(compare.files.join('\n'))}">${esc(names.join(', '))}</b>
      <span class="muted">· ${compare.in_file} UCBs in the file · ${compare.differ} differ (${compare.applicable} can be applied)</span>
      <span class="spacer"></span>
      <button class="primary" id="applyAll" type="button" ${open ? '' : 'disabled'}>Apply all differences</button>
      <button id="closeCompare" type="button">Close compare</button>`;
    $('applyAll').addEventListener('click', () => vscode.postMessage({ type: 'apply', target: 'all' }));
    $('closeCompare').addEventListener('click', () => vscode.postMessage({ type: 'compareClose' }));
  }

  function renderCompareList() {
    const q = $('ucbFilter').value.trim().toLowerCase();
    const only = $('ucbChanged').checked;
    let h = '';
    let group = null;
    for (const u of compare.ucbs) {
      const [cls, tag] = cmpStatus(u);
      if (only && (cls === 'same' || cls === 'absent')) continue;
      if (q && !u.name.toLowerCase().includes(q)) continue;
      if (u.group !== group) {
        group = u.group;
        h += `<div class="grp">${esc(group)}</div>`;
      }
      const dot = cls === 'staged' ? 'staged' : cls === 'diff' ? 'file' : cls === 'ro' ? 'ifx' : '';
      h += `<div class="item ${cls === 'same' || cls === 'absent' ? 'default' : ''} ${u.name === selected ? 'sel' : ''}" data-u="${esc(u.name)}">
        <span class="dot ${dot}"></span><span class="name">${esc(u.name)}</span><span class="tag">${esc(tag)}</span></div>`;
    }
    $('list').innerHTML = h || '<div class="grp">No UCB differs</div>';
    $('list').querySelectorAll('.item').forEach((el) =>
      el.addEventListener('click', () => {
        selected = el.dataset.u;
        vscode.setState({ selected });
        renderCompareList();
        renderCompareDetail();
      }),
    );
  }

  function cmpRow(u, label, name, devText, fileText, differs, opts) {
    let act = '';
    if (differs && opts.auto) act = '<span class="muted">computed</span>';
    else if (differs && u.read_only_reason) act = '<span class="muted">cannot be applied</span>';
    else if (differs && opts.taken) act = `<span class="applied">taken</span><span class="undo" title="Undo" data-revert="${esc(opts.target)}">↺</span>`;
    else if (differs) act = `<button class="small" type="button" data-apply="${esc(opts.target)}">Apply</button>`;
    const cls = !differs ? 'same' : opts.taken ? 'staged' : opts.auto || u.read_only_reason ? 'fixed' : 'diff';
    const follow = differs && opts.auto ? ' <span class="muted">(follows the fields above)</span>' : '';
    return `<div class="cf ${cls}"><div class="lbl">${esc(label)}${name ? `<small>${esc(name)}</small>` : ''}</div>
      <div class="dev mono">${esc(devText)}</div><div class="file mono">${esc(fileText)}${follow}</div><div class="act">${act}</div></div>`;
  }

  function renderCompareDetail() {
    const el = $('detail');
    const u = cmpUcb(selected) || compare.ucbs.find((x) => x.differs) || compare.ucbs[0];
    selected = u.name;
    const [cls] = cmpStatus(u);
    const canApply = u.differs && !u.read_only_reason;
    let h = `<div class="dhead"><h2>UCB_${esc(u.name)}
        ${canApply ? `<button class="small primary" id="applyUcb" type="button" ${u.taken === u.differs ? 'disabled' : ''}>Apply this UCB (${u.differs})</button>` : ''}
        ${u.taken ? '<button class="small" id="revertUcb" type="button">Undo for this UCB</button>' : ''}</h2>
      <div class="line"><span class="pill">${esc({ none: 'not in the file', orig: 'file: ORIG only', copy: 'file: COPY only', both: 'file: ORIG and COPY' }[u.in_file])}</span>
        ${u.read_only_reason ? '<span class="pill">read only</span>' : ''}</div>
      <div class="ffilter"><label class="muted"><input type="checkbox" id="cmpOnlyDiff" ${cmpOnlyDiff ? 'checked' : ''}> Only fields that differ</label></div></div>`;
    if (u.in_file === 'none') {
      h += '<div class="none">The file has no data for this UCB: nothing to compare.</div>';
    } else if (cls === 'same' && cmpOnlyDiff) {
      h += '<div class="none">The file holds exactly what the device has.</div>';
    } else {
      for (const m of u.notes || []) h += `<div class="msg ${m.level === 'warn' ? '' : 'info'}">${esc(m.text)}</div>`;
      h += '<div class="colhead"><span>Field</span><span>On the device</span><span>In the file</span><span></span></div>';
      // The password as one row.
      const pws = u.words.filter(isPw);
      if (pws.length) {
        const fs = pws.map((w) => w.fields[0]);
        const differs = fs.some((f) => f.differs);
        if (differs || !cmpOnlyDiff) {
          const txt = (key) => (fs.some((f) => f[key] === 'unreadable') ? 'unreadable' : fs.some((f) => f[key] === 'set') ? 'set (hidden)' : 'none (all zero)');
          h += `<div class="word"><div class="wh"><b>Password</b><span class="muted mono">+0x100..+0x11C</span></div>${cmpRow(u, 'PW0..PW7', '', txt('dev_text'), differs && txt('dev_text') === txt('file_text') ? `${txt('file_text')}, different` : txt('file_text'), differs, { target: `${u.name}.PW`, taken: differs && fs.filter((f) => f.differs).every((f) => f.taken) })}</div>`;
        }
      }
      for (const w of u.words.filter((x) => !isPw(x))) {
        const fs = w.fields.filter((f) => !cmpOnlyDiff || f.differs);
        if (!fs.length) continue;
        h += `<div class="word"><div class="wh"><b>${esc(w.name)}</b><span class="muted mono">+${esc(w.off)}</span></div>`;
        h += fs
          .map((f) => {
            const target = w.raw ? `${u.name}.${w.name}` : `${u.name}.${w.name}.${f.name}`;
            return cmpRow(u, w.raw ? 'raw word' : f.label, w.raw ? '' : f.name, f.dev_text, f.file_text, f.differs, { target, taken: f.taken, auto: f.kind === 'auto' });
          })
          .join('');
        h += '</div>';
      }
    }
    el.innerHTML = h;
    const a = $('applyUcb');
    if (a) a.addEventListener('click', () => vscode.postMessage({ type: 'apply', target: u.name }));
    const r = $('revertUcb');
    if (r) r.addEventListener('click', () => vscode.postMessage({ type: 'revert', target: u.name }));
    $('cmpOnlyDiff').addEventListener('change', (e) => {
      cmpOnlyDiff = e.target.checked;
      renderCompareDetail();
    });
    el.querySelectorAll('[data-apply]').forEach((b) => b.addEventListener('click', () => vscode.postMessage({ type: 'apply', target: b.dataset.apply })));
    el.querySelectorAll('[data-revert]').forEach((b) => b.addEventListener('click', () => vscode.postMessage({ type: 'revert', target: b.dataset.revert })));
  }

  function renderAll() {
    renderStatus();
    renderCompareBanner();
    $('ucbChanged').parentElement.lastChild.textContent = compare ? ' Only UCBs that differ' : ' Only changed';
    if (compare) {
      renderCompareList();
      renderCompareDetail();
    } else {
      renderList();
      renderDetail();
    }
    renderBar();
  }

  // --- review and write --------------------------------------------------------------------
  function closeModal() {
    $('modalBg').hidden = true;
    $('modal').innerHTML = '';
  }

  function showReview(rv, source) {
    review = rv;
    reviewSource = source;
    const ucbs = rv.changes.length;
    const level = rv.confirm.level;
    const enabled = view ? view.write_enabled : false;
    let h = `<h3>Write UCBs</h3><div class="sub">${ucbs} UCB${ucbs === 1 ? '' : 's'} change${source === 'images' ? ' (from the flash files)' : ''}. Cores are halted and the target is reset afterwards.</div>`;
    h += '<section>';
    if (rv.problems.length) {
      h += rv.problems.map((p) => `<div class="prob ${esc(p.level)}"><b>${p.level === 'block' ? '✖' : p.level === 'warn' ? '⚠' : 'ℹ'} UCB_${esc(p.ucb)}: ${esc(p.title)}</b>${esc(p.detail)}</div>`).join('');
    } else {
      h += '<div class="prob info"><b>✔ No problems found</b></div>';
    }
    h += '</section><section>';
    h += rv.changes
      .map(
        (c) => `<div class="chg"><div class="h"><b>UCB_${esc(c.ucb)}</b><span class="muted">${c.targets.map((t) => `UCB${t.index}: ${esc(t.method)}`).join(', ')}</span><span class="risk ${esc(c.risk)}">${esc(c.risk_text)}</span></div>
        <ul>${c.fields.map((f) => `<li>${esc(f.label)}: ${esc(f.from)} → <span class="to">${esc(f.to)}</span></li>`).join('')}</ul></div>`,
      )
      .join('');
    h += '</section><section>';
    if (rv.notes.length) h += rv.notes.map((n) => `<div class="muted">${esc(n)}</div>`).join('');
    h += `<details><summary>✔ ${rv.passed.length} checks passed</summary><ul>${rv.passed.map((p) => `<li>${esc(p)}</li>`).join('')}</ul></details>`;
    h += `<details><summary>How it is written</summary><ol>
      <li>All 48 UCBs are saved to a backup file first</li>
      <li>Flash images were programmed before; UCBs follow, protection and locks last</li>
      <li>Per UCB pair: COPY first, then ORIG, each read back. One of them is valid at any time.</li>
      <li>Any DMU error stops the write; ORIG keeps its content</li>
      <li>Reset, read again, compare with what was intended</li></ol></details>`;
    if (rv.checks_deferred) h += '<div class="muted">Some checks need the cores halted: they run again when writing.</div>';
    h += '</section><div class="foot">';
    if (rv.blocked) h += '<span class="copy">Fix the problems above to continue.</span>';
    else if (!enabled) h += '<span class="copy">Writing is off: set "ucbWrite": true in the launch configuration.</span>';
    else if (level === 'type') h += `Type <b>${esc(rv.confirm.text)}</b> to confirm: <input type="text" id="typed" size="10" autocomplete="off">`;
    else if (level === 'password') h += '<label><input type="checkbox" id="ack"> I keep the password(s): undoing this needs them</label>';
    else h += '<label><input type="checkbox" id="ack"> I want to write these UCBs</label>';
    h += `<span class="spacer"></span><button id="mCancel" type="button">Cancel</button><button id="mGo" class="danger" type="button" disabled>Write ${ucbs} UCB${ucbs === 1 ? '' : 's'}</button></div>`;
    $('modal').innerHTML = h;
    $('modalBg').hidden = false;
    const go = $('mGo');
    $('mCancel').addEventListener('click', closeModal);
    const typed = $('typed');
    if (typed) typed.addEventListener('input', () => (go.disabled = typed.value !== rv.confirm.text));
    const ack = $('ack');
    if (ack) ack.addEventListener('change', () => (go.disabled = !ack.checked));
    go.addEventListener('click', () => {
      go.disabled = true;
      vscode.postMessage({ type: 'write', token: rv.token, confirm: level === 'type' ? rv.confirm.text : undefined });
      closeModal();
    });
  }

  function showWritten(r) {
    const list = (r.written || []).map((w) => `UCB_${esc(w.ucb)} (UCB${w.index}): ${esc(w.method)}`).join('<br>');
    const after = (r.after || []).map((a) => `UCB_${esc(a.ucb)}: ${esc(a.state)}${a.matches ? ' ✔' : ' ✖ differs'}`).join('<br>');
    $('modal').innerHTML = `<h3>UCBs written</h3><section>${list || 'nothing'}</section><section class="muted">After the reset:<br>${after}</section>
      <section class="muted">Backup: <span class="mono">${esc(r.backup)}</span></section><div class="foot"><span class="spacer"></span><button id="mOk" class="primary" type="button">OK</button></div>`;
    $('modalBg').hidden = false;
    $('mOk').addEventListener('click', closeModal);
  }

  // --- export ------------------------------------------------------------------------------
  // The filters tick the UCBs they match; single UCBs can be ticked or unticked afterwards. The
  // export takes exactly the ticked ones.
  function showExport() {
    if (!view) return;
    const groups = [...new Set(view.ucbs.map((u) => u.class))];
    const readable = (u) => u.words.every((w) => w.readable);
    let h = '<h3>Export UCBs to Intel HEX</h3><div class="sub">Whole 512-byte UCBs. The filters tick the UCBs they match; tick or untick single ones as you like.</div><section>';
    h += `<div class="row"><b>State</b><label><input type="checkbox" class="exS" value="UNLOCKED"> unconfirmed (UNLOCKED)</label><label><input type="checkbox" class="exS" value="CONFIRMED"> confirmed</label></div>`;
    h += `<div class="row"><b>Groups</b>${groups.map((g) => `<label><input type="checkbox" class="exC" value="${esc(g)}"> ${esc(g)}</label>`).join('')}</div>`;
    h += `<div class="row"><label><input type="checkbox" id="exChanged"> only changed from default</label></div>`;
    h += '</section><section>';
    h += `<div class="row"><b>UCBs</b><span class="muted" id="exCount"></span><button class="link" id="exAll" type="button">All</button><button class="link" id="exNone" type="button">None</button></div><div class="sel-grid">`;
    h += view.ucbs
      .map((u) => (readable(u)
        ? `<label><input type="checkbox" class="exU" value="${esc(u.name)}"> ${esc(u.name)}</label>`
        : `<label class="muted" title="Not readable by the debugger"><input type="checkbox" class="exU" value="${esc(u.name)}" disabled> ${esc(u.name)}</label>`))
      .join('');
    h += '</div></section><section>';
    h += `<div class="row"><b>Pairs</b><label><input type="radio" name="exW" value="both" checked> ORIG and COPY</label><label><input type="radio" name="exW" value="orig"> ORIG only</label><label><input type="radio" name="exW" value="copy"> COPY only</label></div>`;
    h += `<div class="row"><label><input type="checkbox" id="exStaged"> with my pending changes</label></div>`;
    h += '</section><div class="foot"><span class="spacer"></span><button id="exCancel" type="button">Cancel</button><button id="exGo" class="primary" type="button">Export…</button></div>';
    $('modal').innerHTML = h;
    $('modalBg').hidden = false;

    const boxes = () => [...document.querySelectorAll('.exU')].filter((x) => !x.disabled);
    const vals = (cls) => [...document.querySelectorAll(cls)].filter((x) => x.checked).map((x) => x.value);
    const count = () => {
      const n = boxes().filter((x) => x.checked).length;
      $('exCount').textContent = `${n} selected`;
      $('exGo').disabled = n === 0;
    };
    // Re-ticks the UCBs from the filters: within a filter any choice matches, the filters together
    // must all match. No filter set: the selection stays as it is.
    const applyFilters = () => {
      const states = vals('.exS');
      const classes = vals('.exC');
      const changed = $('exChanged').checked;
      const staged = $('exStaged').checked;
      if (!states.length && !classes.length && !changed) {
        count();
        return;
      }
      for (const box of boxes()) {
        const u = view.ucbs.find((x) => x.name === box.value);
        box.checked =
          (!states.length || states.includes(u.state)) &&
          (!classes.length || classes.includes(u.class)) &&
          (!changed || u.changed > 0 || (staged && stagedCount(u) > 0));
      }
      count();
    };
    document.querySelectorAll('.exS, .exC, #exChanged').forEach((x) => x.addEventListener('change', applyFilters));
    $('exStaged').addEventListener('change', () => ($('exChanged').checked ? applyFilters() : count()));
    boxes().forEach((x) => x.addEventListener('change', count));
    $('exAll').addEventListener('click', () => {
      boxes().forEach((x) => (x.checked = true));
      count();
    });
    $('exNone').addEventListener('click', () => {
      boxes().forEach((x) => (x.checked = false));
      count();
    });
    count();
    $('exCancel').addEventListener('click', closeModal);
    $('exGo').addEventListener('click', () => {
      const selection = {
        ucbs: vals('.exU'),
        staged: $('exStaged').checked,
        which: document.querySelector('input[name=exW]:checked').value,
      };
      closeModal();
      vscode.postMessage({ type: 'export', selection });
    });
  }

  // --- wiring --------------------------------------------------------------------------------
  $('refresh').addEventListener('click', () => vscode.postMessage(compare ? { type: 'compareAgain', files: compare.files } : { type: 'refresh' }));
  $('compareBtn').addEventListener('click', () => vscode.postMessage({ type: 'compareOpen' }));
  $('exportBtn').addEventListener('click', showExport);
  $('ucbFilter').addEventListener('input', () => (compare ? renderCompareList() : renderList()));
  $('ucbChanged').addEventListener('change', () => (compare ? renderCompareList() : renderList()));
  $('discard').addEventListener('click', () => vscode.postMessage({ type: 'discard' }));
  $('reviewBtn').addEventListener('click', () => vscode.postMessage({ type: 'review' }));

  window.addEventListener('message', (e) => {
    const m = e.data;
    switch (m.type) {
      case 'view':
        view = m.view;
        $('error').hidden = true;
        renderAll();
        break;
      case 'nosession':
        view = null;
        renderAll();
        $('detail').innerHTML = '<div class="none">No active AURIX™ debug session.</div>';
        break;
      case 'busy':
        $('busy').textContent = m.text || '';
        break;
      case 'error':
        $('error').textContent = m.message;
        $('error').hidden = false;
        break;
      case 'review':
        showReview(m.review, m.source || 'staged');
        break;
      case 'written':
        showWritten(m.result);
        break;
      case 'exported':
        break;
      case 'compare':
        if (!compare) {
          // Entering compare mode: show the UCBs that differ, starting with the first one.
          $('ucbChanged').checked = true;
          const first = m.compare.ucbs.find((u) => u.differs && !u.read_only_reason) || m.compare.ucbs.find((u) => u.differs);
          if (first) selected = first.name;
        }
        compare = m.compare;
        if (view) renderAll();
        break;
      case 'compareClosed':
        compare = null;
        $('ucbChanged').checked = false;
        if (view) renderAll();
        break;
    }
  });
})();
