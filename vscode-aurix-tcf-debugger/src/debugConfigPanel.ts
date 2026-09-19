/*
 * Provides a UI for editing and saving debug configuration settings.
 */

import * as vscode from 'vscode';
import { getSvdOptionsForDevice } from './svdCatalog';

function esc(s: string): string {
	return s
		.replaceAll('&', '&amp;')
		.replaceAll('<', '&lt;')
		.replaceAll('>', '&gt;')
		.replaceAll('"', '&quot;')
		.replaceAll("'", '&#39;');
}

// Show the configuration panel.
export async function showDebugConfigPanel(context: vscode.ExtensionContext): Promise<void> {
	const panel = vscode.window.createWebviewPanel(
		'aurixDebugConfig',
		'AURIX™ Debug Configuration',
		vscode.ViewColumn.Active,
		{
			enableScripts: true,
			retainContextWhenHidden: true,
		}
	);

	const folder = vscode.workspace.workspaceFolders?.[0];
	const workspaceFolder = folder?.uri.fsPath || '';

	// Use launch.json ("launch" configuration) as defaults.
	const launchConfigurations = vscode.workspace.getConfiguration('launch', folder?.uri).get<any[]>('configurations') ?? [];
	const launchDefault = launchConfigurations.find((c) => c && typeof c === 'object' && c.type === 'aurixdbg') ?? {};

	const initialSvdOptions = await getSvdOptionsForDevice(context.extensionPath);
	const saved = (context.globalState.get(valuesKey) as Partial<AurixPanelValues> | undefined) ?? {};
	const stringList = (v: unknown): string[] | undefined =>
		Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : undefined;

	const defaults: AurixPanelDefaults = {
		workspaceFolder,
		svdOptions: initialSvdOptions,
		// launch.json values are the defaults; saved values are fallback.
		values: {
			workspace: (launchDefault.workspace ?? saved.workspace ?? workspaceFolder) as string,
			symbolFiles: stringList(launchDefault.symbolFiles) ?? stringList(saved.symbolFiles) ?? [],
			flashFiles: stringList(launchDefault.flashFiles) ?? stringList(saved.flashFiles) ?? [],
			ppuElfPath: ((launchDefault as any).ppuElfPath ?? saved.ppuElfPath ?? '') as string,
			cdspElfPaths: (Array.isArray((saved as any).cdspElfPaths)
				? (saved as any).cdspElfPaths
				: (Array.isArray((launchDefault as any).cdspElfPaths) ? (launchDefault as any).cdspElfPaths : new Array(12).fill(''))) as string[],
			scrElfPath: ((launchDefault as any).scrElfPath ?? saved.scrElfPath ?? '') as string,
			enableFlashing: Boolean((launchDefault as any).enableFlashing ?? (saved as any).enableFlashing ?? false),
			searchDirs: stringList(launchDefault.searchDirs) ?? stringList(saved.searchDirs) ?? [],
			configFiles: stringList(launchDefault.configFiles) ?? stringList(saved.configFiles) ?? [],
			configCommands: stringList(launchDefault.configCommands) ?? stringList(saved.configCommands) ?? [],
			svdFile: (launchDefault.svdFile ?? saved.svdFile ?? '') as string,
			// Default to false: only enable if explicitly requested.
			enableConsoleLog: Boolean(
				(launchDefault as any).enableConsoleLog
					?? (launchDefault as any).debugTCFMessages
					?? (saved as any).enableConsoleLog
					?? (saved as any).debugTCFMessages
					?? false,
			),
			additionalAgentOptions: (launchDefault.additionalAgentOptions ?? saved.additionalAgentOptions ?? '') as string,
		},
	};

	panel.webview.html = getHtml(defaults);

	panel.webview.onDidReceiveMessage(async (msg) => {
		if (!msg || typeof msg !== 'object') {
			return;
		}

		if (msg.type === 'autosave' && msg.config && typeof msg.config === 'object') {
			await savePanelValues(context, msg.config);
			return;
		}

		if (msg.type === 'browseFolder' && typeof (msg as any).key === 'string') {
			const key = String((msg as any).key);
			const picked = await vscode.window.showOpenDialog({
				canSelectMany: false,
				canSelectFiles: false,
				canSelectFolders: true,
				openLabel: 'Select folder',
				defaultUri: folder?.uri,
			});
			const p = picked?.[0]?.fsPath;
			void panel.webview.postMessage({ type: 'browseFolderResult', key, path: p ?? '' });
			return;
		}

		if (msg.type === 'browseElf' && typeof (msg as any).key === 'string') {
			const key = String((msg as any).key);
			const picked = await vscode.window.showOpenDialog({
				canSelectMany: false,
				openLabel: 'Select ELF',
				filters: {
					'ELF files': ['elf'],
					'HEX / MCS assembler files': ['hex', 'asm'],
					'All files': ['*'],
				},
				defaultUri: folder?.uri,
			});
			const p = picked?.[0]?.fsPath;
			void panel.webview.postMessage({ type: 'browseElfResult', key, path: p ?? '' });
			return;
		}

		if (msg.type === 'browseSvd' && typeof (msg as any).key === 'string') {
			const key = String((msg as any).key);
			const picked = await vscode.window.showOpenDialog({
				canSelectMany: false,
				openLabel: 'Select SVD',
				filters: {
					'SVD files': ['svd', 'svd.gz'],
					'All files': ['*'],
				},
				defaultUri: folder?.uri,
			});
			const p = picked?.[0]?.fsPath;
			void panel.webview.postMessage({ type: 'browseSvdResult', key, path: p ?? '' });
			return;
		}

		if (msg.type === 'requestSvdOptions') {
			const options = await getSvdOptionsForDevice(context.extensionPath);
			void panel.webview.postMessage({ type: 'svdOptions', options });
			return;
		}

		if (msg.type === 'apply' && msg.config && typeof msg.config === 'object') {
			await savePanelValues(context, msg.config);
			return;
		}

		if (msg.type === 'start' && msg.config && typeof msg.config === 'object') {
			// Save last-used values so reopening the panel keeps them.
			await savePanelValues(context, msg.config);

			const folder = vscode.workspace.workspaceFolders?.[0];
			await vscode.debug.startDebugging(folder, msg.config as vscode.DebugConfiguration);
		}
	}, undefined, context.subscriptions);
}

const valuesKey = 'aurix-debugger.debugConfigPanel.values';

type AurixPanelValues = {
	workspace: string;
	symbolFiles: string[];
	flashFiles: string[];
	ppuElfPath: string;
	cdspElfPaths: string[];
	scrElfPath: string;
	enableFlashing: boolean;
	searchDirs: string[];
	configFiles: string[];
	configCommands: string[];
	svdFile: string;
	enableConsoleLog: boolean;
	additionalAgentOptions: string;
};

type AurixPanelDefaults = {
	workspaceFolder: string;
	svdOptions: Array<{ label: string; value: string }>;
	values: AurixPanelValues;
};

// Persist the last-used panel values.
async function savePanelValues(context: vscode.ExtensionContext, config: any): Promise<void> {
	const cfg = config as any;
	const enableConsoleLog = Boolean(
		(cfg as any).enableConsoleLog ??
		(cfg as any).debugTCFMessages
	);
	const toSave: AurixPanelValues = {
		workspace: typeof cfg.workspace === 'string' ? cfg.workspace : '',
		symbolFiles: Array.isArray(cfg.symbolFiles) ? cfg.symbolFiles.filter((v: any) => typeof v === 'string') : [],
		flashFiles: Array.isArray(cfg.flashFiles) ? cfg.flashFiles.filter((v: any) => typeof v === 'string') : [],
		ppuElfPath: typeof (cfg as any).ppuElfPath === 'string' ? (cfg as any).ppuElfPath : '',
		cdspElfPaths: Array.isArray((cfg as any).cdspElfPaths)
			? (cfg as any).cdspElfPaths.map((v: any) => typeof v === 'string' ? v : '')
			: new Array(12).fill(''),
		scrElfPath: typeof (cfg as any).scrElfPath === 'string' ? (cfg as any).scrElfPath : '',
		enableFlashing: Boolean((cfg as any).enableFlashing),
		searchDirs: Array.isArray(cfg.searchDirs) ? cfg.searchDirs.filter((v: any) => typeof v === 'string') : [],
		configFiles: Array.isArray(cfg.configFiles) ? cfg.configFiles.filter((v: any) => typeof v === 'string') : [],
		configCommands: Array.isArray(cfg.configCommands) ? cfg.configCommands.filter((v: any) => typeof v === 'string') : [],
		svdFile: typeof cfg.svdFile === 'string' ? cfg.svdFile : '',
		enableConsoleLog,
		additionalAgentOptions: typeof cfg.additionalAgentOptions === 'string' ? cfg.additionalAgentOptions : '',
	};
	await context.globalState.update(valuesKey, toSave);
}

// Build the HTML content for the webview.
function getHtml(defaults: AurixPanelDefaults): string {
	const projectValue = esc(defaults.values.workspace || defaults.workspaceFolder || '');
	const ppuElfValue = esc(defaults.values.ppuElfPath || '');
	const scrElfValue = esc(defaults.values.scrElfPath || '');
	const cdspElfValues = Array.isArray(defaults.values.cdspElfPaths) ? defaults.values.cdspElfPaths : new Array(12).fill('');
	const enableFlashingChecked = defaults.values.enableFlashing ? 'checked' : '';
	const lines = (v: string[] | undefined) => esc((Array.isArray(v) ? v : []).join('\n'));
	const symbolFilesValue = lines(defaults.values.symbolFiles);
	const flashFilesValue = lines(defaults.values.flashFiles);
	const searchDirsValue = lines(defaults.values.searchDirs);
	const configFilesValue = lines(defaults.values.configFiles);
	const configCommandsValue = lines(defaults.values.configCommands);
	const svdValue = esc(defaults.values.svdFile || '');
	const enableConsoleLogChecked = defaults.values.enableConsoleLog ? 'checked' : '';
	const additionalAgentOptionsValue = esc(defaults.values.additionalAgentOptions || '');
	const svdRawValue = defaults.values.svdFile || '';
	const hasSvdPresetMatch = defaults.svdOptions.some((option) => option.value === svdRawValue);
	const svdPresetOptions = [
		`<option value="" ${svdRawValue ? '' : 'selected'}>Auto</option>`,
		...defaults.svdOptions.map((option) => {
			const selected = option.value === svdRawValue ? ' selected' : '';
			return `<option value="${esc(option.value)}"${selected}>${esc(option.label)}</option>`;
		}),
		`<option value="__custom__" ${svdRawValue && !hasSvdPresetMatch ? 'selected' : ''}>Custom path...</option>`,
	].join('');

	const cdspRows = new Array(12).fill(0).map((_, i) => {
		const value = esc(String(cdspElfValues[i] ?? ''));
		return `
			<div class="grid3Row">
				<label for="cdspElf${i}">CDSP${i}</label>
				<input id="cdspElf${i}" placeholder="C:\\path\\to\\cdsp${i}.elf" value="${value}" />
				<button type="button" class="browseElf" data-key="cdspElf${i}">Browse...</button>
			</div>`;
	}).join('');

	return `<!doctype html>
<html lang="en">
<head>
	<meta charset="utf-8" />
	<meta name="viewport" content="width=device-width,initial-scale=1" />
	<title>AURIX™ Debug Configuration</title>
	<style>
		:root { color-scheme: light dark; }
		body {
			font-family: var(--vscode-font-family);
			font-size: var(--vscode-font-size);
			color: var(--vscode-foreground);
			background: var(--vscode-editor-background);
			padding: 16px;
		}
		.tabs {
			display: flex;
			gap: 8px;
			margin: 14px 0 12px;
			flex-wrap: wrap;
		}
		.tabs button {
			background: var(--vscode-button-secondaryBackground);
			color: var(--vscode-button-secondaryForeground);
		}
		.tabs button.active {
			background: var(--vscode-button-background);
			color: var(--vscode-button-foreground);
		}
		.tab {
			display: none;
		}
		.tab.active {
			display: block;
		}
		.grid {
			display: grid;
			grid-template-columns: 180px 1fr;
			gap: 10px 12px;
			align-items: center;
			max-width: 900px;
		}
		.group {
			max-width: 900px;
			border: 1px solid var(--vscode-panel-border);
			border-radius: 6px;
			padding: 12px;
			margin: 12px 0;
		}
		.groupTitle {
			font-weight: 600;
			margin-bottom: 10px;
		}
		.grid3Row {
			display: grid;
			grid-template-columns: 140px 1fr 90px;
			gap: 10px 12px;
			align-items: center;
			margin: 8px 0;
		}
		.inlineRow {
			display: flex;
			gap: 10px;
			align-items: center;
		}
		.inlineRow > input,
		.inlineRow > select {
			flex: 1 1 auto;
		}
		.inlineRow > button {
			flex: 0 0 auto;
		}
		.inlineRow .checkbox {
			flex: 0 0 auto;
			white-space: nowrap;
		}
		label { color: var(--vscode-foreground); }
		input, select, textarea {
			width: 100%;
			box-sizing: border-box;
			padding: 6px 8px;
			border-radius: 4px;
			border: 1px solid var(--vscode-input-border);
			background: var(--vscode-input-background);
			color: var(--vscode-input-foreground);
		}
		textarea { min-height: 70px; resize: vertical; }
		.row { grid-column: 1 / -1; }
		.actions {
			margin-top: 16px;
			display: flex;
			gap: 10px;
		}
		button {
			padding: 6px 12px;
			border-radius: 4px;
			border: 1px solid var(--vscode-button-border, transparent);
			background: var(--vscode-button-background);
			color: var(--vscode-button-foreground);
			cursor: pointer;
		}
		button.secondary {
			background: var(--vscode-button-secondaryBackground);
			color: var(--vscode-button-secondaryForeground);
		}
		.small {
			font-size: 0.9em;
			color: var(--vscode-descriptionForeground);
		}
		.checkbox {
			display: flex;
			gap: 8px;
			align-items: center;
		}
		.checkbox input { width: auto; }
	</style>
</head>
<body>
	<h2>AURIX™ Debug Configuration</h2>
	<div class="small">Starts a debug session without editing launch.json.</div>

	<div class="tabs" role="tablist">
		<button type="button" class="tabBtn active" data-tab="project">Project</button>
		<button type="button" class="tabBtn" data-tab="cores">PPU/CDSP/SCR Cores</button>
	</div>

	<div class="tab active" id="tab-project">
		<div class="grid">
			<label for="project">Project</label>
			<div class="inlineRow">
				<input id="project" placeholder="C:\\path\\to\\project" value="${projectValue}" />
				<button type="button" class="browseFolder" data-key="project">Browse...</button>
			</div>

			<label for="symbolFiles">Symbol files</label>
			<div class="inlineRow">
				<textarea id="symbolFiles" placeholder="One ELF file per line: the application, bootloader, HSM, ...">${symbolFilesValue}</textarea>
				<button type="button" class="browseElf" data-key="symbolFiles">Add...</button>
			</div>

			<label for="flashFiles">Flash files</label>
			<div class="inlineRow">
				<div class="checkbox" title="Programs the flash files with aurix-dbg before starting the debug session">
					<input type="checkbox" id="enableFlashing" ${enableFlashingChecked} />
					<span class="small">Enable flashing</span>
				</div>
				<textarea id="flashFiles" placeholder="One ELF, HEX or binary file per line">${flashFilesValue}</textarea>
				<button type="button" class="browseElf" data-key="flashFiles">Add...</button>
			</div>

			<label for="searchDirs">Script search directories</label>
			<textarea id="searchDirs" placeholder="One directory per line (OpenOCD -s)">${searchDirsValue}</textarea>

			<label for="configFiles">Configuration scripts</label>
			<textarea id="configFiles" placeholder="One file per line, e.g. board/infineon/kit_a2g_tc377tx_3v3_trb.cfg (OpenOCD -f)">${configFilesValue}</textarea>

			<label for="configCommands">Tcl commands</label>
			<textarea id="configCommands" placeholder="One command per line, run after the scripts (OpenOCD -c)">${configCommandsValue}</textarea>

			<label for="svdPreset">SVD selection</label>
			<div class="inlineRow">
				<select id="svdPreset">${svdPresetOptions}</select>
			</div>

			<label for="svdFile">SVD file</label>
			<div class="inlineRow">
				<input id="svdFile" placeholder="C:\\path\\to\\device.svd" value="${svdValue}" />
				<button type="button" class="browseSvd" data-key="svdFile">Browse...</button>
			</div>

			<label for="enableConsoleLog">Enable console log</label>
			<div class="checkbox">
				<input type="checkbox" id="enableConsoleLog" ${enableConsoleLogChecked} />
				<span class="small">Show debug server protocol messages in the Debug Console</span>
			</div>
		</div>
	</div>

	<div class="tab" id="tab-cores">
		<div class="group">
			<div class="groupTitle">PPU (Parallel Processing Unit)</div>
			<div class="grid3Row">
				<label for="ppuElfPath">ELF File</label>
				<input id="ppuElfPath" placeholder="C:\\path\\to\\ppu.elf" value="${ppuElfValue}" />
				<button type="button" class="browseElf" data-key="ppuElfPath">Browse...</button>
			</div>
		</div>

		<div class="group">
			<div class="groupTitle">CDSP (Converter Digital Signal Processor) - 12 cores</div>
			${cdspRows}
		</div>

		<div class="group">
			<div class="groupTitle">SCR (Standby Controller) (reserved)</div>
			<div class="grid3Row">
				<label for="scrElfPath">ELF File</label>
				<input id="scrElfPath" placeholder="C:\\path\\to\\scr.elf" value="${scrElfValue}" />
				<button type="button" class="browseElf" data-key="scrElfPath">Browse...</button>
			</div>
		</div>

		<div class="group">
			<div class="groupTitle">Additional aurix-dbg Options</div>
			<div class="small" style="margin-bottom: 8px;">Command-line options appended as-is, e.g. -d3 -l aurix-dbg.log</div>
			<textarea id="additionalAgentOptions" placeholder="Custom parameters">${additionalAgentOptionsValue}</textarea>
		</div>
	</div>

	<div class="row actions" style="max-width:900px;">
		<button id="apply" class="secondary">Apply</button>
		<button id="start">Debug</button>
		<button id="close" class="secondary">Cancel</button>
	</div>

	<script>
		const vscode = acquireVsCodeApi();

		function v(id) { return document.getElementById(id); }
		function bool(id) { return v(id).checked === true; }
		function str(id) { return String(v(id).value || '').trim(); }
		function list(id) { return String(v(id).value || '').split(/\\r?\\n/).map((x) => x.trim()).filter((x) => x); }
		const SVD_CUSTOM = '__custom__';

		function upsertSvdPresetOptions(options) {
			const select = v('svdPreset');
			const svdInput = v('svdFile');
			if (!select || !svdInput) return;

			const oldValue = String(select.value || '');
			const currentInput = String(svdInput.value || '').trim();
			const normalized = Array.isArray(options)
				? options
					.filter((item) => item && typeof item === 'object')
					.map((item) => ({
						label: String(item.label || item.value || ''),
						value: String(item.value || ''),
					}))
					.filter((item) => item.value)
				: [];

			select.innerHTML = '';

			for (const item of normalized) {
				const opt = document.createElement('option');
				opt.value = item.value;
				opt.textContent = item.label || item.value;
				select.appendChild(opt);
			}

			const custom = document.createElement('option');
			custom.value = SVD_CUSTOM;
			custom.textContent = 'Custom path...';
			select.appendChild(custom);

			const containsOld = Array.from(select.options).some((o) => o.value === oldValue);
			if (containsOld) {
				select.value = oldValue;
			} else if (currentInput && Array.from(select.options).some((o) => o.value === currentInput)) {
				select.value = currentInput;
			} else if (currentInput) {
				select.value = SVD_CUSTOM;
			} else {
				select.value = '';
			}
		}

		function syncSvdFromPreset() {
			const select = v('svdPreset');
			const svdInput = v('svdFile');
			if (!select || !svdInput) return;
			const preset = String(select.value || '');
			if (preset === SVD_CUSTOM) {
				return;
			}
			svdInput.value = preset;
		}
		function buildConfig() {
			const config = {
				type: 'aurixdbg',
				request: 'launch',
				name: 'Launch (GUI)',
				host: '127.0.0.1',
				port: 1534,
				autoStartAgent: true,
				stopOnEntry: true,
				resumeOtherCoresOnEntry: false,
			};

			config.symbolFiles = list('symbolFiles');
			config.flashFiles = list('flashFiles');
			config.enableFlashing = bool('enableFlashing');
			config.searchDirs = list('searchDirs');
			config.configFiles = list('configFiles');
			config.configCommands = list('configCommands');
			const svdFile = str('svdFile');
			if (svdFile) config.svdFile = svdFile;
			const project = str('project');
			if (project) config.workspace = project;
			config.enableConsoleLog = bool('enableConsoleLog');
			config.additionalAgentOptions = str('additionalAgentOptions');

			const ppuElfPath = str('ppuElfPath');
			if (ppuElfPath) config.ppuElfPath = ppuElfPath;
			const scrElfPath = str('scrElfPath');
			if (scrElfPath) config.scrElfPath = scrElfPath;
			const cdspElfPaths = [];
			let anyCdsp = false;
			for (let i = 0; i < 12; i++) {
				const p = str('cdspElf' + i);
				cdspElfPaths.push(p);
				if (p) anyCdsp = true;
			}
			if (anyCdsp) config.cdspElfPaths = cdspElfPaths;

			return config;
		}

		function selectTab(name) {
			for (const b of document.querySelectorAll('.tabBtn')) {
				b.classList.toggle('active', b.dataset.tab === name);
			}
			for (const t of document.querySelectorAll('.tab')) {
				t.classList.toggle('active', t.id === 'tab-' + name);
			}
		}

		for (const b of document.querySelectorAll('.tabBtn')) {
			b.addEventListener('click', () => selectTab(b.dataset.tab));
		}

		for (const b of document.querySelectorAll('button.browseElf')) {
			b.addEventListener('click', () => {
				const key = b.dataset.key;
				if (!key) return;
				vscode.postMessage({ type: 'browseElf', key });
			});
		}
		for (const b of document.querySelectorAll('button.browseFolder')) {
			b.addEventListener('click', () => {
				const key = b.dataset.key;
				if (!key) return;
				vscode.postMessage({ type: 'browseFolder', key });
			});
		}
		for (const b of document.querySelectorAll('button.browseSvd')) {
			b.addEventListener('click', () => {
				const key = b.dataset.key;
				if (!key) return;
				vscode.postMessage({ type: 'browseSvd', key });
			});
		}

		const svdPresetSelect = v('svdPreset');
		if (svdPresetSelect) {
			svdPresetSelect.addEventListener('change', () => {
				syncSvdFromPreset();
				scheduleAutosave();
			});
		}

		window.addEventListener('message', (event) => {
			const msg = event.data;
			if (!msg || typeof msg !== 'object') return;
			if (msg.type === 'browseElfResult' && typeof msg.key === 'string') {
				const el = document.getElementById(msg.key);
				const picked = String(msg.path || '');
				if (el && el.tagName === 'TEXTAREA') {
					// File lists: add the picked file as a new line.
					if (picked) {
						const current = String(el.value || '').replace(/\\s+$/, '');
						el.value = current ? current + '\\n' + picked : picked;
						scheduleAutosave();
					}
				} else if (el && 'value' in el) {
					el.value = picked;
					scheduleAutosave();
				}
			}
			if (msg.type === 'browseFolderResult' && typeof msg.key === 'string') {
				const el = document.getElementById(msg.key);
				if (el && 'value' in el) {
					el.value = String(msg.path || '');
					scheduleAutosave();
				}
			}
			if (msg.type === 'browseSvdResult' && typeof msg.key === 'string') {
				const el = document.getElementById(msg.key);
				if (el && 'value' in el) {
					el.value = String(msg.path || '');
					upsertSvdPresetOptions([]);
					scheduleAutosave();
				}
			}
			if (msg.type === 'svdOptions') {
				upsertSvdPresetOptions(msg.options);
				scheduleAutosave();
			}
		});

		vscode.postMessage({ type: 'requestSvdOptions' });

		let autosaveTimer;
		function scheduleAutosave() {
			if (autosaveTimer) clearTimeout(autosaveTimer);
			autosaveTimer = setTimeout(() => {
				const config = buildConfig();
				vscode.postMessage({ type: 'autosave', config });
			}, 250);
		}

		for (const el of document.querySelectorAll('input, textarea, select')) {
			el.addEventListener('input', scheduleAutosave);
			el.addEventListener('change', scheduleAutosave);
		}

		v('apply').addEventListener('click', () => {
			const config = buildConfig();
			vscode.postMessage({ type: 'apply', config });
		});

		v('start').addEventListener('click', () => {
			const config = buildConfig();
			vscode.postMessage({ type: 'start', config });
		});

		v('close').addEventListener('click', () => window.close());
	</script>
</body>
</html>`;
}
