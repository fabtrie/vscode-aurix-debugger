/*
 * AURIX Debugger VSCode Extension
 * Based on Intel's vscode-tcf-debug
 * Copyright (C) 2025 Infineon Technologies AG
 * 
 * Entry point for the VSCode extension.
 */
import * as vscode from 'vscode';
import * as path from 'path';
import * as net from 'net';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { AurixTCFDebugSession, DisassemblyOpcodeBytes, isDisassemblyOpcodeBytes } from './debugProvider';
import { ChildProcess, execFile, spawn } from 'child_process';
import { pipeline } from 'stream/promises';
import { createGunzip } from 'zlib';
import { LiveWatchCommandArgument, LiveWatchProvider } from './liveWatchView';
import { peripheralWatchEntryFromNode } from './peripheralLiveWatch';
import { findSvdForDevice } from './svdCatalog';
import { registerDebuggerActions } from './debuggerActions';
import { VsCodeBreakpointUi } from './vscodeBreakpointUi';

let tcfAgentProcess: ChildProcess | undefined;
let extensionContext: vscode.ExtensionContext;
let agentOutput: vscode.OutputChannel | undefined;
let activeAurixSessions = 0;
let startingAurixSessions = 0;
let pendingStopTimer: NodeJS.Timeout | undefined;
let pendingStopToken = 0;
let lastAgentHost: string | undefined;
let lastAgentPort: number | undefined;
let lastAgentPath: string | undefined;
let enforcingMemoryInspectorSettings = false;
const memoryInspectorExtensionId = 'eclipse-cdt.memory-inspector';
const openedDisassemblySessions = new Set<string>();
const aurixDebugSessions = new Set<vscode.DebugSession>();
let resolvedDisassemblyOpenCommand: string | undefined;
let instructionStepStatusItem: vscode.StatusBarItem | undefined;
const instructionStepStateKey = 'aurix-debugger.instructionSteppingEnabled';
let lastInstructionSteppingEnabled = false;
let liveWatchStatusItem: vscode.StatusBarItem | undefined;
let liveWatchProvider: LiveWatchProvider | undefined;
const liveWatchIntervalStateKey = 'aurix-debugger.liveWatch.intervalMs';
let lastLiveWatchIntervalMs = 1000;
let preferredSourceViewColumn: vscode.ViewColumn | undefined;
let lastAutoSourcePath: string | undefined;
let lastAutoSourceLine: number | undefined;
const SVD_CACHE_FOLDER = 'svd-cache';
let warnedSvdFileReplaced = false;
const DISASSEMBLY_OPEN_DELAY_MS = 1200;
const pendingDisassemblyOpenTimers = new Map<string, NodeJS.Timeout>();
const LIVE_WATCH_INTERVAL_OPTIONS = [200, 500, 1000, 2000] as const;

export function activate(context: vscode.ExtensionContext) {
	// This method is called when the extension is activated
	extensionContext = context;
	console.log('AURIX debugger extension is activating...');
	agentOutput = vscode.window.createOutputChannel('AURIX Debug Server');
	liveWatchProvider = new LiveWatchProvider(context);
	context.subscriptions.push(liveWatchProvider);

	void ensureMemoryInspectorSettings();
	context.subscriptions.push(
		vscode.workspace.onDidChangeConfiguration((event) => {
			if (enforcingMemoryInspectorSettings) {
				return;
			}
			if (
				event.affectsConfiguration('memory-inspector.debugTypes') ||
				event.affectsConfiguration('memory-inspector.refreshOnStop') ||
				event.affectsConfiguration('memory-inspector.periodicRefresh') ||
				event.affectsConfiguration('memory-inspector.periodicRefreshInterval')
			) {
				void ensureMemoryInspectorSettings();
			}
			if (event.affectsConfiguration('aurix-debugger.disassembly.opcodeBytes')) {
				const opcodeBytes = getDisassemblyOpcodeBytes();
				for (const s of aurixDebugSessions) {
					void Promise.resolve(s.customRequest('aurix/setDisassemblyOptions', { opcodeBytes })).catch(() => undefined);
				}
			}
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.resetTarget', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurixdbg') {
				void vscode.window.showWarningMessage('No active AURIX™ debug session.');
				return;
			}
			// The launch configuration decides: "resetSuspend": true halts at the reset vector,
			// otherwise the target runs after reset and is paused afterwards (pauseAll), since
			// some firmware only relaxes its watchdog after a startup check that runs a short
			// time after reset.
			await s.customRequest('aurix/reset', {});
		})
	);

	let flashing = false;
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.flashTarget', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurixdbg') {
				void vscode.window.showWarningMessage('No active AURIX™ debug session.');
				return;
			}
			if (flashing) {
				void vscode.window.showWarningMessage('AURIX™ Debugger: flashing is already running.');
				return;
			}
			flashing = true;
			try {
				await vscode.window.withProgress(
					{ location: vscode.ProgressLocation.Notification, title: 'AURIX™ Debugger: flashing target (see Debug Console)...' },
					async () => { await s.customRequest('aurix/flash', {}); }
				);
				// Same as the Reset button: the launch configuration decides whether the target halts or runs.
				await s.customRequest('aurix/reset', {});
				void vscode.window.showInformationMessage('AURIX™ Debugger: flashing done.');
			} catch (e) {
				void vscode.window.showErrorMessage(`AURIX™ Debugger: flashing failed: ${e instanceof Error ? e.message : String(e)}`);
			} finally {
				flashing = false;
			}
		})
	);

	registerDebuggerActions(context);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.showResetCapabilities', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurixdbg') {
				void vscode.window.showWarningMessage('No active AURIX™ debug session.');
				return;
			}
			const result = await vscode.window.withProgress(
				{ location: vscode.ProgressLocation.Notification, title: 'Querying ContextReset.getCapabilities...' },
				() => s.customRequest('aurix/resetCapabilities')
			);
			const caps = Array.isArray(result?.capabilities) ? result.capabilities : [];
			if (result?.error) {
				void vscode.window.showErrorMessage(`ContextReset.getCapabilities failed: ${result.error}`);
			} else if (caps.length === 0) {
				void vscode.window.showInformationMessage('The debug server reported no reset types. See the "AURIX Debug Server" output or the Debug Console.');
			} else {
				const summary = caps.map((c: { Type: string; Description?: string }) => c.Description ? `${c.Type} (${c.Description})` : c.Type).join(', ');
				void vscode.window.showInformationMessage(`Supported reset types: ${summary}`);
			}
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.toggleInstructionStepping', async () => {
			const enabled = !(await getInstructionSteppingEnabled());
			await setInstructionSteppingMode(enabled);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.disassemblyGoTo', async (target?: string) => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurixdbg') {
				void vscode.window.showWarningMessage('No active AURIX™ debug session.');
				return;
			}
			const picked = typeof target === 'string' ? target : await pickDisassemblyTarget(s);
			if (!picked) {
				return;
			}
			await goToDisassemblyTarget(s, picked);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.configureLiveWatch', async () => {
			const currentInterval = await getLiveWatchIntervalMs();
			const items: Array<vscode.QuickPickItem & { intervalMs?: number; custom?: boolean }> = [
				...LIVE_WATCH_INTERVAL_OPTIONS.map((intervalMs) => ({
					label: `${intervalMs} ms`,
					description: currentInterval === intervalMs ? 'Current' : '',
					intervalMs,
				})),
				{
					label: 'Custom...',
					description: !LIVE_WATCH_INTERVAL_OPTIONS.includes(currentInterval as any) ? `Current: ${currentInterval} ms` : '',
					custom: true,
				},
			];
			const picked = await vscode.window.showQuickPick(items, {
				title: 'Configure Live Watch',
				placeHolder: 'Select the Live Watch refresh interval while the target runs (minimum 200ms+)',
				ignoreFocusOut: true,
			});
			if (!picked) {
				return;
			}
			if (picked.custom) {
				const customValue = await vscode.window.showInputBox({
					title: 'Custom Live Watch Interval',
					placeHolder: 'Enter refresh interval in milliseconds (minimum 200ms+)',
					value: String(currentInterval),
					ignoreFocusOut: true,
					validateInput: (input) => {
						const trimmed = input.trim();
						if (trimmed === '') {
							return 'Interval is required.';
						}
						const parsed = Number(trimmed);
						if (!Number.isFinite(parsed) || parsed < 200) {
							return 'Minimum 200ms+';
						}
						return undefined;
					},
				});
				if (customValue === undefined) {
					return;
				}
				await setLiveWatchInterval(Number(customValue.trim()));
				return;
			}
			await setLiveWatchInterval(picked.intervalMs ?? currentInterval);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchAddExpression', async () => {
			await liveWatchProvider?.addExpression(await pickLiveWatchExpression());
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchRemoveExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.removeExpression(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchSetValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setValue(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchPauseExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setExpressionPaused(item, true);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchResumeExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setExpressionPaused(item, false);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchAddPeripheral', async (node?: unknown) => {
			const entry = peripheralWatchEntryFromNode(node);
			if (!entry) {
				void vscode.window.showWarningMessage('Select a register or field in the XPeripherals view to add it to the Live Watch.');
				return;
			}
			if (entry.hasReadSideEffect) {
				const add = 'Add Anyway';
				const answer = await vscode.window.showWarningMessage(
					`Reading ${entry.label} has a side effect (e.g. clear on read). The Live Watch reads it on every refresh.`,
					{ modal: true },
					add
				);
				if (answer !== add) {
					return;
				}
			}
			await liveWatchProvider?.addPeripheral(entry);
			await vscode.commands.executeCommand(`${LiveWatchProvider.viewType}.focus`);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchEditExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.editExpression(item);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchCopyValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.copyValue(item);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchCopyExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.copyExpression(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchClearExpressions', async () => {
			await liveWatchProvider?.clearExpressions();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchRefresh', async () => {
			await liveWatchProvider?.refreshNow();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.enableInstructionStepping', async () => {
			await setInstructionSteppingMode(true);
		}),
		vscode.commands.registerCommand('aurix-debugger.disableInstructionStepping', async () => {
			await setInstructionSteppingMode(false);
		})
	);

	instructionStepStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
	instructionStepStatusItem.command = 'aurix-debugger.toggleInstructionStepping';
	instructionStepStatusItem.tooltip = 'Toggle instruction-level stepping';
	context.subscriptions.push(instructionStepStatusItem);
	liveWatchStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
	liveWatchStatusItem.command = 'aurix-debugger.configureLiveWatch';
	liveWatchStatusItem.tooltip = 'Configure the Live Watch refresh interval';
	context.subscriptions.push(liveWatchStatusItem);
	context.subscriptions.push(vscode.window.registerWebviewViewProvider(LiveWatchProvider.viewType, liveWatchProvider));
	// Like the Watch view, the Live Watch updates on every break, also for paused expressions and when
	// the periodic refresh is off. The adapter's stopped event also covers cores stopping without focus.
	context.subscriptions.push(
		vscode.debug.registerDebugAdapterTrackerFactory('aurixdbg', {
			createDebugAdapterTracker: () => ({
				onDidSendMessage: (message: { type?: string; event?: string }) => {
					if (message?.type === 'event' && message.event === 'stopped') {
						liveWatchProvider?.refreshOnBreak();
					}
				},
			}),
		})
	);

	// Register debug configuration provider
	context.subscriptions.push(
		vscode.debug.registerDebugConfigurationProvider('aurixdbg', new AurixTCFConfigurationProvider())
	);

	// Register debug adapter factory
	context.subscriptions.push(
		vscode.debug.registerDebugAdapterDescriptorFactory('aurixdbg', new AurixTCFDebugAdapterFactory())
	);

	// Register timestamp command (useful for recording TCF messages with timestamps)
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.timestamp', () => {
			return new Date().toISOString().replaceAll(":", "-");
		})
	);

	// Ensure we clean up the agent when the debug session is terminated.
	context.subscriptions.push(
		vscode.debug.onDidStartDebugSession((session: vscode.DebugSession) => {
			if (session.type === 'aurixdbg') {
				activeAurixSessions++;
				aurixDebugSessions.add(session);
				if (pendingStopTimer) {
					clearTimeout(pendingStopTimer);
					pendingStopTimer = undefined;
					pendingStopToken++;
					agentOutput?.appendLine('Cancelled pending agent shutdown (new session started).');
				}
				void ensureMemoryInspectorSettings();
				void syncInstructionSteppingOnSessionStart(session);
				void syncLiveWatchOnSessionStart(session);
				void liveWatchProvider?.refreshNow();
			}
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidChangeActiveDebugSession(() => {
			void liveWatchProvider?.refreshNow();
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidChangeActiveStackItem((item) => {
			const session = vscode.debug.activeDebugSession;
			if (!session || session.type !== 'aurixdbg') {
				return;
			}
			// The adapter needs the focused thread to tell the toolbar pause button (which pauses the
			// focused thread) from a core's pause button in the Call Stack view.
			const focused = item && item.session.id === session.id ? item.threadId : undefined;
			focusFileLog(session, `activeStackItem thread=${String(focused)} frame=${String((item as vscode.DebugStackFrame | undefined)?.frameId)}`);
			void Promise.resolve(session.customRequest('aurix/activeThread', { threadId: focused })).catch(() => undefined);
			if (!item) {
				return;
			}
			// Only act once we have a stack frame (stopped state).
			const isStackFrame = typeof (item as vscode.DebugStackFrame).frameId === 'number';
			if (!isStackFrame) {
				return;
			}
			// Open the disassembly layout once the source of the first focused frame is shown, so it
			// lands next to the source editor. The timer is only a fallback for frames without source.
			if (!openedDisassemblySessions.has(session.id) && !pendingDisassemblyOpenTimers.has(session.id)) {
				const timer = setTimeout(() => {
					pendingDisassemblyOpenTimers.delete(session.id);
					openDisassemblyLayoutOnce(session);
				}, DISASSEMBLY_OPEN_DELAY_MS);
				pendingDisassemblyOpenTimers.set(session.id, timer);
			}
			void autoRevealSourceForStackItem(item as vscode.DebugStackFrame).then((shown) => {
				if (shown) {
					openDisassemblyLayoutOnce(session);
				}
			});
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidReceiveDebugSessionCustomEvent((e) => {
			if (e.session.type !== 'aurixdbg') {
				return;
			}
			if (e.event !== 'aurix/sourceLocation') {
				return;
			}
			const body = e.body as { path?: string; line?: number } | undefined;
			const srcPath = body?.path;
			const line = typeof body?.line === 'number' ? body.line : undefined;
			if (!srcPath || line === undefined) {
				return;
			}
			focusFileLog(e.session, `sourceLocation event ${srcPath}:${line}`);
			void autoRevealSourcePath(srcPath, line);
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidTerminateDebugSession(async (session) => {
			if (session.type !== 'aurixdbg') {
				return;
			}
			const openTimer = pendingDisassemblyOpenTimers.get(session.id);
			if (openTimer) {
				clearTimeout(openTimer);
				pendingDisassemblyOpenTimers.delete(session.id);
			}
			openedDisassemblySessions.delete(session.id);
			aurixDebugSessions.delete(session);
			activeAurixSessions =Math.max(0, activeAurixSessions - 1);
			if (activeAurixSessions === 0) {
				updateInstructionSteppingStatus(lastInstructionSteppingEnabled, false);
				updateLiveWatchStatus(false);
				liveWatchProvider?.refreshScheduling(false, lastLiveWatchIntervalMs);
			}
			// Use a short, cancelable delay so VS Code Restart (terminate + immediate start)
			// can reuse the same agent without kill+start churn.
			if (activeAurixSessions === 0) {
				if (pendingStopTimer) {
					clearTimeout(pendingStopTimer);
				}
				const stopToken = ++pendingStopToken;
				agentOutput?.appendLine('Scheduling agent shutdown in 1s (cancelled if a new session starts).');
				pendingStopTimer = setTimeout(() => {
					pendingStopTimer = undefined;
					if (
						stopToken === pendingStopToken &&
						activeAurixSessions === 0 &&
						startingAurixSessions === 0
					) {
						void stopTCFAgent('debug session terminated');
					}
				}, 1000);
			}
		})
	);

	// Handle agent lifecycle
	context.subscriptions.push({
		dispose: () => {
			void stopTCFAgent('extension disposed');
		}
	});

	void vscode.window.showInformationMessage('AURIX debugger extension is enabled.');
}

export class AurixTCFDebugAdapterFactory implements vscode.DebugAdapterDescriptorFactory {
    // Create the debug adapter descriptor for a debug session
	async createDebugAdapterDescriptor(session: vscode.DebugSession): Promise<vscode.ProviderResult<vscode.DebugAdapterDescriptor>> {
		startingAurixSessions++;
		// VS Code Restart can create a new session/adapter before firing onDidStartDebugSession.
		// Cancel any pending shutdown early so we don't kill the agent mid-start.
		if (pendingStopTimer) {
			clearTimeout(pendingStopTimer);
			pendingStopTimer = undefined;
			pendingStopToken++;
			agentOutput?.appendLine('Cancelled pending agent shutdown (session is starting).');
		}
		try {
		const config = session.configuration;

		if (config.autoStartAgent) {
			await startAurixDbg(session, config);
		}

		// Enable path mapper based on workspace trust and configuration
		const pathMapperConfigDefault = vscode.workspace.getConfiguration("aurixdbg").inspect("executePathMapper")?.defaultValue ?? true;
		const pathMapperConfig = vscode.workspace.getConfiguration("aurixdbg").get('executePathMapper', pathMapperConfigDefault);

		const enablePathMapper = vscode.workspace.isTrusted && pathMapperConfig === true;
		
		return new vscode.DebugAdapterInlineImplementation(new AurixTCFDebugSession(enablePathMapper, getDisassemblyOpcodeBytes(), new VsCodeBreakpointUi()));
		} finally {
			startingAurixSessions = Math.max(0, startingAurixSessions - 1);
		}
	}
}

function resolveConfigPath(session: vscode.DebugSession, p: unknown): string | undefined {
	// Resolve ${workspaceFolder} and make absolute
	if (typeof p !== 'string' || p.trim() === '') {
		return undefined;
	}
	let out = p;
	const ws = session.workspaceFolder?.uri.fsPath || vscode.workspace.workspaceFolders?.[0]?.uri.fsPath || '';
	if (ws) {
		out = out.replaceAll('${workspaceFolder}', ws);
		out = out.replaceAll('${workspaceRoot}', ws);
	}
	// Normalize separators for Windows tools.
	out = out.replace(/\//g, path.sep);
	if (!path.isAbsolute(out) && ws) {
		out = path.resolve(ws, out);
	}
	return out;
}


function splitCommandLine(text: string): string[] {
	// Simple quote-aware split for Additional Agent Options.
	// Supports: foo bar "quoted arg" 'single quoted'.
	const out: string[] = [];
	let cur = '';
	let quote: '"' | "'" | undefined;
	for (let i = 0; i < text.length; i++) {
		const ch = text[i];
		if (quote) {
			if (ch === quote) {
				quote = undefined;
				continue;
			}
			cur += ch;
			continue;
		}
		if (ch === '"' || ch === "'") {
			quote = ch as any;
			continue;
		}
		if (ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r') {
			if (cur.trim() !== '') {
				out.push(cur);
				cur = '';
			}
			continue;
		}
		cur += ch;
	}
	if (cur.trim() !== '') {
		out.push(cur);
	}
	return out;
}

function getSvdCacheRoot(): string {
	const fromGlobalStorage = extensionContext?.globalStorageUri?.fsPath;
	if (typeof fromGlobalStorage === 'string' && fromGlobalStorage.trim() !== '') {
		return path.join(fromGlobalStorage, SVD_CACHE_FOLDER);
	}
	return path.join(extensionContext.extensionPath, '.cache', SVD_CACHE_FOLDER);
}

async function ensureDirectory(dirPath: string): Promise<void> {
	await fs.promises.mkdir(dirPath, { recursive: true });
}

async function materializeCompressedSvd(svdGzPath: string): Promise<string> {
	const normalizedInput = path.normalize(svdGzPath);
	if (!fs.existsSync(normalizedInput)) {
		throw new Error(`Compressed SVD not found: ${normalizedInput}`);
	}

	const stat = await fs.promises.stat(normalizedInput);
	const baseName = path.basename(normalizedInput, '.gz');
	const digest = crypto
		.createHash('sha256')
		.update(`${baseName}:${stat.size}`)
		.digest('hex')
		.slice(0, 16);

	const cacheDir = getSvdCacheRoot();
	await ensureDirectory(cacheDir);

	const outputPath = path.join(cacheDir, `${baseName}.${digest}`);
	if (fs.existsSync(outputPath)) {
		return outputPath;
	}

	const tempPath = `${outputPath}.tmp`;
	agentOutput?.appendLine(`Preparing SVD cache: ${baseName}`);
	await vscode.window.withProgress(
		{
			location: vscode.ProgressLocation.Notification,
			title: 'AURIX™ Debugger: preparing SVD definitions (first run)',
			cancellable: false,
		},
		async (progress) => {
			progress.report({ message: baseName });
			await pipeline(
				fs.createReadStream(normalizedInput),
				createGunzip(),
				fs.createWriteStream(tempPath)
			);
		}
	);
	await fs.promises.rename(tempPath, outputPath);
	agentOutput?.appendLine(`SVD cache ready: ${outputPath}`);
	return outputPath;
}

/** Path of a bundled SVD file the peripheral viewer can read (.svd.gz files are unpacked to the cache). */
async function usableSvdPath(svdPath: string): Promise<string> {
	return svdPath.toLowerCase().endsWith('.svd.gz') ? await materializeCompressedSvd(svdPath) : svdPath;
}

export class AurixTCFConfigurationProvider implements vscode.DebugConfigurationProvider {

	/**
	 * Provide initial debug configurations when launch.json is created
	 */
	provideDebugConfigurations(
		folder: vscode.WorkspaceFolder | undefined,
		token?: vscode.CancellationToken
	): vscode.ProviderResult<vscode.DebugConfiguration[]> {
		return [
			{
				type: 'aurixdbg',
				request: 'launch',
				name: 'Debug Aurix Program',
				workspace: '${workspaceFolder}',
				autoStartAgent: true,
				searchDirs: ['${workspaceFolder}/tcl'],
				configFiles: ['board/infineon/kit_a2g_tc377tx_3v3_trb.cfg'],
				device: 'TC37x',
				symbolFiles: ['${workspaceFolder}/build/program.elf'],
				flashFiles: ['${workspaceFolder}/build/program.elf'],
				enableFlashing: false,
				enableConsoleLog: false
			}
		];
	}

	/**
	 * Massage a debug configuration just before a debug session is being launched.
	 */
	async resolveDebugConfiguration(
		folder: vscode.WorkspaceFolder | undefined, 
		config: vscode.DebugConfiguration, 
		token?: vscode.CancellationToken
	): Promise<vscode.DebugConfiguration | undefined> {

		// This extension does not support user-facing attach sessions.
		if ((config as any).request === 'attach') {
			void vscode.window.showErrorMessage('AURIX™ Debugger: attach is not supported. Use a launch configuration.');
			return undefined;
		}

		if ((config as any).program !== undefined) {
			void vscode.window.showErrorMessage('AURIX™ Debugger: "program" is no longer supported. List the ELF files in "symbolFiles" (and in "flashFiles" to flash them).');
			return undefined;
		}

		// If launch.json is missing or empty
		if (!config.type && !config.request && !config.name) {
			const editor = vscode.window.activeTextEditor;
			if (editor && (editor.document.languageId === 'cpp' || editor.document.languageId === 'c')) {
				config.type = 'aurixdbg';
				config.name = 'Launch Aurix Program';
				config.request = 'launch';
				config.host = 'localhost';
				config.port = 1534;
			}
		}

		// Provide defaults
		if (!config.host) {
			config.host = 'localhost';
		}
		if (!config.port) {
			config.port = 1534;
		}

		// Console logging toggle: enableConsoleLog maps to debugTCFMessages.
		if (typeof (config as any).enableConsoleLog === 'boolean') {
			(config as any).debugTCFMessages = (config as any).enableConsoleLog;
		}

		// The XPeripherals view (mcu-debug.peripheral-viewer) reads "svdPath" when the session
		// starts; "device" selects the SVD file bundled with the extension. This does not
		// touch the target, which may still be unpowered.
		if ((config as any).svdFile !== undefined && !warnedSvdFileReplaced) {
			warnedSvdFileReplaced = true;
			void vscode.window.showWarningMessage('AURIX™ Debugger: "svdFile" is no longer supported and is ignored. Set "device" (e.g. "TC37x") to select the SVD file bundled with the extension.');
		}
		delete (config as any).svdFile;
		delete (config as any).svdPath;
		const device = typeof config.device === 'string' ? config.device.trim() : '';
		if (device) {
			const entry = await findSvdForDevice(extensionContext.extensionPath, device);
			if (entry) {
				try {
					(config as any).svdPath = await usableSvdPath(entry.absolutePath);
				} catch (e) {
					agentOutput?.appendLine(`SVD for ${device} unavailable: ${e instanceof Error ? e.message : String(e)}`);
				}
			} else {
				void vscode.window.showWarningMessage(`AURIX™ Debugger: unknown device "${device}"; the XPeripherals view is unavailable.`);
			}
		}

		return config;
	}
}

/**
 * Starts aurix-dbg (OpenOCD-compatible debug server over Infineon TAS) with
 * OpenOCD-style arguments built from the launch configuration:
 * -s <searchDirs> -f <configFiles> -c <configCommands>.
 * Uses the aurix-dbg.exe bundled with the extension unless "agentPath" overrides it.
 */
async function startAurixDbg(session: vscode.DebugSession, config: vscode.DebugConfiguration): Promise<void> {
	const agentPath = resolveConfigPath(session, config.agentPath || vscode.workspace.getConfiguration('aurixdbg').get('agentPath'))
		?? path.join(extensionContext.extensionPath, 'bin', 'aurix-dbg.exe');
	if (!fs.existsSync(agentPath)) {
		void vscode.window.showErrorMessage(`aurix-dbg not found: ${agentPath}`);
		throw new Error(`aurix-dbg not found: ${agentPath}`);
	}
	const host = String(config.host || '127.0.0.1');
	const port = Number(config.port || 1534);
	const asList = (v: unknown): string[] => {
		if (Array.isArray(v)) {
			return v.filter((x): x is string => typeof x === 'string' && x.trim() !== '');
		}
		return typeof v === 'string' && v.trim() !== '' ? [v] : [];
	};

	const args: string[] = [];
	for (const dir of asList(config.searchDirs)) {
		args.push('-s', resolveConfigPath(session, dir) ?? dir);
	}
	for (const file of asList(config.configFiles)) {
		// Names like "board/infineon/x.cfg" are looked up in the search dirs by aurix-dbg;
		// only pass a resolved path when the file exists relative to the workspace.
		const resolved = resolveConfigPath(session, file);
		args.push('-f', resolved && fs.existsSync(resolved) ? resolved : file);
	}
	for (const cmd of asList(config.configCommands)) {
		args.push('-c', cmd);
	}
	args.push('-c', `bindto ${host === 'localhost' ? '127.0.0.1' : host}`, '-c', `tcf_port ${port}`);
	if (typeof config.additionalAgentOptions === 'string') {
		args.push(...splitCommandLine(config.additionalAgentOptions));
	}
	if (Array.isArray(config.agentArgs)) {
		args.push(...asList(config.agentArgs));
	}

	let reused = false;
	if (tcfAgentProcess && tcfAgentProcess.exitCode === null) {
		agentOutput?.appendLine('Reusing already running aurix-dbg for new session.');
		try {
			await waitForTcpPort(host, port, 15_000);
			reused = true;
		} catch (e) {
			agentOutput?.appendLine(`Running aurix-dbg is not reachable (${e instanceof Error ? e.message : String(e)}); restarting it.`);
		}
	}
	if (!reused) {
		await startTCFAgent(agentPath, args, host, port);
	}
}

async function startTCFAgent(agentPath: string, args: string[], host: string, port: number): Promise<void> {
	// Stop existing agent if running (should be rare now; we prefer reusing it)
	await stopTCFAgent('restart requested');

	lastAgentHost = host;
	lastAgentPort = port;
	lastAgentPath = agentPath;

	agentOutput?.show(true);
	agentOutput?.appendLine(`Starting aurix-dbg: ${agentPath}`);
	// Quoted, so the line can be pasted into a shell as-is. Unquoted, a single
	// argument like "bindto 127.0.0.1" looks like two and the agent rejects it with
	// "unknown option: 127.0.0.1" - spawn() itself passes it as one argument.
	agentOutput?.appendLine(`Agent command line: ${[agentPath, ...args].map(quoteCommandLineArg).join(' ')}`);

	const proc = spawn(agentPath, args, {
		cwd: path.dirname(agentPath),
		stdio: ['ignore', 'pipe', 'pipe'],
		windowsHide: true,
	});
	tcfAgentProcess = proc;

	// The agent says on stdout/stderr why it refused to start (bad option, cfg script
	// error, probe busy). That goes to the output channel, but a failing launch pulls
	// the panel over to the Debug Console, so keep the tail and put it into the thrown
	// error as well - that is what the user actually gets shown.
	const recentOutput: string[] = [];
	const record = (line: string) => {
		agentOutput?.appendLine(line);
		recentOutput.push(line);
		if (recentOutput.length > 200) {
			recentOutput.shift();
		}
	};
	// Chunks are not lines: split them so partial lines are not logged as whole ones.
	const pump = (stream: NodeJS.ReadableStream | null) => {
		let pending = '';
		stream?.on('data', (data) => {
			pending += String(data);
			const lines = pending.split(/\r?\n/);
			pending = lines.pop() ?? '';
			for (const line of lines) {
				record(line);
			}
		});
		stream?.on('end', () => {
			if (pending !== '') {
				record(pending);
				pending = '';
			}
		});
	};
	pump(proc.stdout);
	pump(proc.stderr);

	// A failed spawn (e.g. ENOENT) emits 'error' but never sets exitCode, so without this
	// waitForTcpPort would poll until its timeout and hide the real cause.
	const spawnFailed = new Promise<never>((_, reject) => {
		proc.once('error', (err) => {
			if (tcfAgentProcess === proc) {
				tcfAgentProcess = undefined;
			}
			reject(new Error(`Failed to start aurix-dbg '${agentPath}': ${err.message}`));
		});
	});
	spawnFailed.catch(() => { /* surfaced via Promise.race below */ });

	// 'close', not 'exit': 'exit' can fire before the last stdout/stderr chunks have been
	// delivered, and those chunks are the failure reason.
	const closed = new Promise<number | null>((resolve) => proc.once('close', (code) => resolve(code)));
	const exitedEarly = closed.then((code): never => {
		throw new Error(`aurix-dbg exited with code ${code} before listening on ${host}:${port}`);
	});
	exitedEarly.catch(() => { /* surfaced via Promise.race below */ });

	proc.on('exit', (code, signal) => {
		agentOutput?.appendLine(`aurix-dbg exited with code ${code}${signal ? ` (signal ${signal})` : ''}`);
		if (tcfAgentProcess === proc) {
			tcfAgentProcess = undefined;
		}
	});

	proc.on('error', (err) => {
		agentOutput?.appendLine(`Failed to start aurix-dbg: ${err.message}`);
	});

	// Avoid a race where the debug adapter tries to connect before the agent has
	// started listening on the requested TCP port.
	agentOutput?.appendLine(`Waiting for aurix-dbg to listen on ${host}:${port}...`);
	try {
		await Promise.race([waitForTcpPort(host, port, 15_000), spawnFailed, exitedEarly]);
	} catch (e) {
		// If the process is already gone, let its streams drain so the reported tail is
		// complete (waitForTcpPort can reject on 'exit', i.e. before 'close').
		if (proc.exitCode !== null || proc.signalCode !== null) {
			await Promise.race([closed, new Promise((resolve) => setTimeout(resolve, 500))]);
		}
		const message = `${e instanceof Error ? e.message : String(e)}${formatAgentOutputTail(recentOutput)}`;
		agentOutput?.appendLine(`aurix-dbg did not start listening: ${message}`);
		agentOutput?.show(true);
		// Don't leave a hung agent behind: it keeps the probe locked and later launches would reuse it.
		await stopTCFAgent('startup failed');
		throw new Error(message);
	}
	agentOutput?.appendLine(`aurix-dbg is listening on ${host}:${port}`);
}

/** Quotes an argument for display, so a logged command line can be pasted into a shell. */
function quoteCommandLineArg(arg: string): string {
	return /[\s"]/.test(arg) ? `"${arg.replace(/(\\*)"/g, '$1$1\\"')}"` : arg;
}

/** The last non-empty lines the agent printed, for appending to a startup error. */
function formatAgentOutputTail(lines: string[], max = 15): string {
	const tail = lines.map((l) => l.trimEnd()).filter((l) => l.trim() !== '').slice(-max);
	return tail.length === 0 ? '' : `\n\nAgent output:\n${tail.join('\n')}`;
}

async function stopTCFAgent(reason: string): Promise<void> {
	// Stop the TCF agent process if running
	if (!tcfAgentProcess) {
		return;
	}

	const proc = tcfAgentProcess;
	const pid = proc.pid;
	tcfAgentProcess = undefined;

	if (!pid) {
		try {
			proc.kill();
		} catch {
			// ignore
		}
		return;
	}

	agentOutput?.appendLine(`Stopping aurix-dbg (pid ${pid}) - ${reason}`);

	try {
		if (process.platform === 'win32') {
			// Kill the whole process tree, including helper processes the agent may have spawned.
			await new Promise<void>((resolve, reject) => {
				execFile('taskkill', ['/PID', String(pid), '/T', '/F'], { windowsHide: true }, (err, stdout, stderr) => {
					if (stdout) {
						agentOutput?.appendLine(String(stdout).trimEnd());
					}
					if (stderr) {
						agentOutput?.appendLine(String(stderr).trimEnd());
					}
					if (err) {
						reject(err);
						return;
					}
					resolve();
				});
			});
		} else {
			// Best-effort on non-Windows.
			proc.kill();
		}
	} catch (e) {
		// Fall back to a plain kill.
		try {
			proc.kill();
		} catch {
			// ignore
		}
		agentOutput?.appendLine(`Failed to stop aurix-dbg cleanly: ${String(e)}`);
	}

	// Give the OS a moment to release the port.
	await new Promise(resolve => setTimeout(resolve, 300));
	if (lastAgentHost && typeof lastAgentPort === 'number') {
		try {
			await waitForTcpPortClosed(lastAgentHost, lastAgentPort, 10_000);
		} catch {
			// Some agents detach/spawn a helper that keeps listening after the parent PID is killed.
			// Keep this simple: if the port is still open, force-kill the agent image name.
			if (process.platform === 'win32') {
				// If a new session already started (Restart), do NOT do the image-name kill,
				// otherwise we might kill the new agent we just launched/reused.
				if (activeAurixSessions > 0 || startingAurixSessions > 0) {
					agentOutput?.appendLine('Port still open, but a session is active/starting; skipping taskkill by image name.');
					return;
				}
				const image = path.basename(lastAgentPath ?? 'aurix-dbg.exe');
				agentOutput?.appendLine(`Port still open after taskkill by PID; forcing taskkill by image name ${image}`);
				try {
					await new Promise<void>((resolve, reject) => {
						execFile('taskkill', ['/IM', image, '/T', '/F'], { windowsHide: true }, (err, stdout, stderr) => {
							if (stdout) {
								agentOutput?.appendLine(String(stdout).trimEnd());
							}
							if (stderr) {
								agentOutput?.appendLine(String(stderr).trimEnd());
							}
							if (err) {
								reject(err);
								return;
							}
							resolve();
						});
					});
				} catch (e) {
					agentOutput?.appendLine(`Failed to taskkill ${image} by image name: ${String(e)}`);
				}
			}
		}
	}
}

function waitForTcpPortClosed(host: string, port: number, timeoutMs: number): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	return new Promise((resolve, reject) => {
		const tryOnce = () => {
			const socket = new net.Socket();
			socket.setTimeout(500);
			socket.once('connect', () => {
				socket.destroy();
				retryOrFail();
			});
			socket.once('timeout', () => {
				socket.destroy();
				// timeout means no clean refusal; keep trying until deadline
				retryOrFail();
			});
			socket.once('error', () => {
				socket.destroy();
				// ECONNREFUSED/ENOTFOUND => port not listening
				resolve();
			});

			const retryOrFail = () => {
				if (Date.now() >= deadline) {
					reject(new Error(`Timed out waiting for ${host}:${port} to close`));
					return;
				}
				setTimeout(tryOnce, 150);
			};

			socket.connect(port, host);
		};
		tryOnce();
	});
}

function waitForTcpPort(host: string, port: number, timeoutMs: number): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	return new Promise((resolve, reject) => {
		const tryOnce = () => {
			// If the agent already exited, don't keep polling.
			if (!tcfAgentProcess) {
				reject(new Error('aurix-dbg is not running'));
				return;
			}
			if (tcfAgentProcess.exitCode !== null) {
				reject(new Error(`aurix-dbg exited early (code ${tcfAgentProcess.exitCode})`));
				return;
			}
			const socket = new net.Socket();
			socket.setTimeout(500);
			socket.once('connect', () => {
				socket.destroy();
				resolve();
			});
			socket.once('timeout', () => {
				socket.destroy();
				retryOrFail();
			});
			socket.once('error', () => {
				socket.destroy();
				retryOrFail();
			});

			const retryOrFail = () => {
				if (Date.now() >= deadline) {
					reject(new Error(`Timed out waiting for ${host}:${port}`));
					return;
				}
				setTimeout(tryOnce, 150);
			};

			socket.connect(port, host);
		};

		tryOnce();
	});
}

async function ensureMemoryInspectorSettings(): Promise<void> {
	const cfg = vscode.workspace.getConfiguration();
	const desiredDebugger = 'aurixdbg';
	const currentTypes = cfg.get<string[]>('memory-inspector.debugTypes') ?? [];
	const typeList = Array.isArray(currentTypes) ? currentTypes.slice() : [];
	const needsType = !typeList.includes(desiredDebugger);
	if (!needsType) {
		return;
	}
	try {
		enforcingMemoryInspectorSettings = true;
		typeList.push(desiredDebugger);
		await cfg.update('memory-inspector.debugTypes', typeList, vscode.ConfigurationTarget.Workspace);
	} catch (e) {
		agentOutput?.appendLine(`Failed to enforce memory-inspector settings: ${String(e)}`);
	} finally {
		enforcingMemoryInspectorSettings = false;
	}
}

function openDisassemblyLayoutOnce(session: vscode.DebugSession): void {
	const pending = pendingDisassemblyOpenTimers.get(session.id);
	if (pending) {
		clearTimeout(pending);
		pendingDisassemblyOpenTimers.delete(session.id);
	}
	const activeSession = vscode.debug.activeDebugSession;
	const alive = !!activeSession && activeSession.id === session.id && activeSession.type === 'aurixdbg';
	if (!alive || openedDisassemblySessions.has(session.id)) {
		return;
	}
	openedDisassemblySessions.add(session.id);
	void openDisassemblyEditorLayout(session);
}

async function openDisassemblyEditorLayout(session: vscode.DebugSession): Promise<void> {
	if (session.type !== 'aurixdbg') {
		return;
	}
	const cfg = vscode.workspace.getConfiguration('aurixdbg');
	const autoOpen = cfg.get<boolean>('autoOpenDisassemblyView', true);
	if (!autoOpen) {
		return;
	}
	const autoLock = cfg.get<boolean>('autoLockDisassemblyView', true);
	const autoEven = cfg.get<boolean>('autoEvenEditorWidths', true);

	// Preserve the current editor focus so we can return the user to source.
	const startEditor = vscode.window.activeTextEditor;
	const startViewColumn = startEditor?.viewColumn;
	const startIsPreview = startEditor ? isPreviewTab(startEditor.document.uri, startViewColumn) : false;
	if (startViewColumn !== undefined) {
		preferredSourceViewColumn = startViewColumn;
	}

	try {
		const cmd = await resolveDisassemblyOpenCommand();
		if (!cmd) {
			throw new Error('No disassembly command found in VS Code command registry.');
		}
		await vscode.commands.executeCommand(cmd);
	} catch (e) {
		agentOutput?.appendLine(`Failed to open disassembly view: ${String(e)}`);
		return;
	}

	try {
		await vscode.commands.executeCommand('workbench.action.moveEditorToRightGroup');
	} catch (e) {
		agentOutput?.appendLine(`Failed to move disassembly view to the right: ${String(e)}`);
	}

	if (autoLock) {
		try {
			const commands = await vscode.commands.getCommands(true);
			if (commands.includes('workbench.action.unlockEditorGroup')) {
				await vscode.commands.executeCommand('workbench.action.unlockEditorGroup');
			}
			await vscode.commands.executeCommand('workbench.action.lockEditorGroup');
		} catch (e) {
			agentOutput?.appendLine(`Failed to lock disassembly editor group: ${String(e)}`);
		}
	}

	if (autoEven) {
		try {
			await vscode.commands.executeCommand('workbench.action.evenEditorWidths');
		} catch (e) {
			agentOutput?.appendLine(`Failed to even editor widths: ${String(e)}`);
		}
	}

	if (startEditor && startViewColumn) {
		try {
			await vscode.window.showTextDocument(startEditor.document, {
				viewColumn: startViewColumn,
				// Keep a preview tab (opened by the stop reveal) in preview mode instead of pinning it.
				preview: startIsPreview,
				preserveFocus: false
			});
		} catch (e) {
			agentOutput?.appendLine(`Failed to restore source editor focus: ${String(e)}`);
		}
	}
}

/** Quick pick listing the program's functions; any other text is sent as an address or symbol. */
function pickDisassemblyTarget(session: vscode.DebugSession): Promise<string | undefined> {
	return pickSymbolOrText({
		title: 'Disassembly: Go to Address or Symbol',
		placeholder: 'Address (0x80001234) or symbol (main, main+0x10)',
		typedDescription: 'Go to address or symbol',
		session,
		symbolsRequest: 'aurix/disassemblySymbols',
	});
}

function pickLiveWatchExpression(): Promise<string | undefined> {
	const session = vscode.debug.activeDebugSession;
	return pickSymbolOrText({
		title: 'Add Live Watch Expression',
		placeholder: 'Pick a variable or type an expression, e.g. counter_6 or *(uint32_t*)0x70000044',
		typedDescription: 'Add expression',
		session: session?.type === 'aurixdbg' ? session : undefined,
		symbolsRequest: 'aurix/variableSymbols',
	});
}

/**
 * Quick pick listing the program's symbols (from the adapter) that also accepts any typed text: the
 * typed text is always offered as the first entry unless it matches a symbol exactly.
 */
async function pickSymbolOrText(options: {
	title: string;
	placeholder: string;
	typedDescription: string;
	session: vscode.DebugSession | undefined;
	symbolsRequest: string;
}): Promise<string | undefined> {
	const qp = vscode.window.createQuickPick<vscode.QuickPickItem>();
	qp.title = options.title;
	qp.placeholder = options.placeholder;
	qp.matchOnDescription = true;
	qp.busy = options.session !== undefined;
	let symbolItems: vscode.QuickPickItem[] = [];
	const update = () => {
		const text = qp.value.trim();
		const exact = symbolItems.some((i) => i.label === text);
		// alwaysShow keeps the typed entry visible although it does not match any symbol.
		qp.items = text && !exact
			? [{ label: text, description: options.typedDescription, alwaysShow: true }, ...symbolItems]
			: symbolItems;
	};
	if (options.session) {
		void Promise.resolve(options.session.customRequest(options.symbolsRequest))
			.then((r) => {
				const symbols: Array<{ name: string; address: string; size?: number }> = Array.isArray(r?.symbols) ? r.symbols : [];
				symbolItems = symbols.map((sym) => ({
					label: sym.name,
					description: sym.size ? `${sym.address} · ${sym.size} byte${sym.size === 1 ? '' : 's'}` : sym.address,
				}));
			})
			.catch(() => undefined)
			.finally(() => {
				qp.busy = false;
				update();
			});
	}
	return new Promise((resolve) => {
		qp.onDidChangeValue(update);
		qp.onDidAccept(() => {
			resolve(qp.selectedItems[0]?.label ?? (qp.value.trim() || undefined));
			qp.hide();
		});
		qp.onDidHide(() => {
			resolve(undefined);
			qp.dispose();
		});
		qp.show();
	});
}

/**
 * VS Code's Disassembly view has no API to show an address; it follows the focused stack frame.
 * The adapter adds a frame for the target at the bottom of the call stack, which is focused here.
 */
async function goToDisassemblyTarget(session: vscode.DebugSession, target: string): Promise<void> {
	// Refreshing the call stacks refocuses the top frame; wait for that before focusing the target frame.
	let settle: () => void = () => undefined;
	const settled = new Promise<void>((resolve) => { settle = resolve; });
	let quietTimer: NodeJS.Timeout | undefined;
	const maxTimer = setTimeout(() => settle(), 2000);
	const listener = vscode.debug.onDidChangeActiveStackItem(() => {
		if (quietTimer) {
			clearTimeout(quietTimer);
		}
		quietTimer = setTimeout(() => settle(), 200);
	});
	try {
		const result = await session.customRequest('aurix/disassemblyGoTo', { target });
		await settled;
		await vscode.commands.executeCommand('workbench.action.debug.callStackBottom');
		const cmd = await resolveDisassemblyOpenCommand();
		if (cmd) {
			await vscode.commands.executeCommand(cmd);
		}
		vscode.window.setStatusBarMessage(`Disassembly: ${String(result?.label ?? target)}`, 5000);
	} catch (e) {
		void vscode.window.showErrorMessage(`Go to address or symbol: ${e instanceof Error ? e.message : String(e)}`);
	} finally {
		listener.dispose();
		clearTimeout(maxTimer);
		if (quietTimer) {
			clearTimeout(quietTimer);
		}
	}
}

function isPreviewTab(uri: vscode.Uri, viewColumn: vscode.ViewColumn | undefined): boolean {
	const norm = normalizePathForCompare(uri.fsPath);
	for (const group of vscode.window.tabGroups.all) {
		if (viewColumn !== undefined && group.viewColumn !== viewColumn) {
			continue;
		}
		for (const tab of group.tabs) {
			const input = tab.input;
			if (input instanceof vscode.TabInputText && normalizePathForCompare(input.uri.fsPath) === norm) {
				return tab.isPreview;
			}
		}
	}
	return false;
}

async function resolveDisassemblyOpenCommand(): Promise<string | undefined> {
	if (resolvedDisassemblyOpenCommand) {
		return resolvedDisassemblyOpenCommand;
	}
	const preferred = ['debug.action.openDisassemblyView', 'debug.openDisassemblyView', 'workbench.action.debug.openDisassemblyView'];
	try {
		const commands = await vscode.commands.getCommands(true);
		for (const cmd of preferred) {
			if (commands.includes(cmd)) {
				resolvedDisassemblyOpenCommand = cmd;
				return cmd;
			}
		}
		const fallback = commands.find((cmd) => /disassembly/i.test(cmd) && /(open|view)/i.test(cmd));
		if (fallback) {
			resolvedDisassemblyOpenCommand = fallback;
			return fallback;
		}
		agentOutput?.appendLine(
			`Disassembly command not found. Available disassembly-related commands: ${commands.filter(c => /disassembly/i.test(c)).join(', ')}`
		);
	} catch (e) {
		agentOutput?.appendLine(`Failed to resolve disassembly command: ${String(e)}`);
	}
	return undefined;
}

async function getInstructionSteppingEnabled(): Promise<boolean> {
	const stored = extensionContext.workspaceState.get<boolean>(instructionStepStateKey);
	if (typeof stored === 'boolean') {
		return stored;
	}
	const cfg = vscode.workspace.getConfiguration('aurixdbg');
	return cfg.get<boolean>('instructionSteppingDefault', false);
}

function getDisassemblyOpcodeBytes(): DisassemblyOpcodeBytes {
	const value = vscode.workspace.getConfiguration('aurixdbg').get('disassembly.opcodeBytes');
	return isDisassemblyOpcodeBytes(value) ? value : 'left';
}

function normalizeLiveWatchIntervalMs(value: unknown): number {
	const numeric = typeof value === 'number' ? value : Number(value);
	if (!Number.isFinite(numeric)) {
		return 1000;
	}
	return Math.min(10_000, Math.max(200, Math.round(numeric)));
}

async function getLiveWatchIntervalMs(): Promise<number> {
	const stored = extensionContext.workspaceState.get<number>(liveWatchIntervalStateKey);
	if (typeof stored === 'number' && Number.isFinite(stored)) {
		return normalizeLiveWatchIntervalMs(stored);
	}
	const cfg = vscode.workspace.getConfiguration('aurixdbg');
	return normalizeLiveWatchIntervalMs(cfg.get<number>('liveWatch.intervalMs', 1000));
}

/** Stores the Live Watch refresh interval and applies it while an AURIX session runs. */
async function setLiveWatchInterval(value: number): Promise<void> {
	lastLiveWatchIntervalMs = normalizeLiveWatchIntervalMs(value);
	await extensionContext.workspaceState.update(liveWatchIntervalStateKey, lastLiveWatchIntervalMs);
	const sessionActive = activeAurixSessions > 0;
	updateLiveWatchStatus(sessionActive);
	liveWatchProvider?.refreshScheduling(sessionActive, lastLiveWatchIntervalMs);
}

function updateLiveWatchStatus(visible: boolean): void {
	if (!liveWatchStatusItem) {
		return;
	}
	liveWatchStatusItem.text = `Live Watch: ${lastLiveWatchIntervalMs}ms`;
	liveWatchStatusItem.tooltip = `Live Watch refreshes every ${lastLiveWatchIntervalMs} ms while the target runs. Click to change.`;
	if (visible) {
		liveWatchStatusItem.show();
	} else {
		liveWatchStatusItem.hide();
	}
}

async function setInstructionSteppingEnabled(value: boolean): Promise<void> {
	lastInstructionSteppingEnabled = value;
	await extensionContext.workspaceState.update(instructionStepStateKey, value);
}

async function setInstructionSteppingMode(enabled: boolean, session?: vscode.DebugSession): Promise<void> {
	await setInstructionSteppingEnabled(enabled);
	const targetSession = session ?? vscode.debug.activeDebugSession;
	if (targetSession && targetSession.type === 'aurixdbg') {
		try {
			await targetSession.customRequest('aurix/setInstructionStepping', { enabled });
		} catch (e) {
			agentOutput?.appendLine(`Failed to set instruction stepping: ${String(e)}`);
		}
	}
	updateInstructionSteppingStatus(enabled, true);
}

function updateInstructionSteppingStatus(enabled: boolean, visible: boolean): void {
	lastInstructionSteppingEnabled = enabled;
	void vscode.commands.executeCommand('setContext', 'aurix-debugger.instructionStepping', enabled);
	if (!instructionStepStatusItem) {
		return;
	}
	instructionStepStatusItem.text = enabled ? 'Instr Step: ON' : 'Instr Step: OFF';
	instructionStepStatusItem.tooltip = enabled
		? 'Instruction stepping enabled (click to disable)'
		: 'Instruction stepping disabled (click to enable)';
	if (visible) {
		instructionStepStatusItem.show();
	} else {
		instructionStepStatusItem.hide();
	}
}

async function syncInstructionSteppingOnSessionStart(session: vscode.DebugSession): Promise<void> {
	if (session.type !== 'aurixdbg') {
		return;
	}
	const enabled = await getInstructionSteppingEnabled();
	await setInstructionSteppingMode(enabled, session);
}

async function syncLiveWatchOnSessionStart(session: vscode.DebugSession): Promise<void> {
	if (session.type !== 'aurixdbg') {
		return;
	}
	const sessionConfig = session.configuration as { liveWatchIntervalMs?: number } | undefined;
	const intervalMs = sessionConfig?.liveWatchIntervalMs !== undefined
		? normalizeLiveWatchIntervalMs(sessionConfig.liveWatchIntervalMs)
		: await getLiveWatchIntervalMs();
	await setLiveWatchInterval(intervalMs);
}

/** Returns true when the frame's source location is shown in an editor. */
async function autoRevealSourceForStackItem(item: vscode.DebugStackFrame): Promise<boolean> {
	const frame = item as any;
	const source = frame?.source as { path?: string } | undefined;
	if (!source || !source.path) {
		return false;
	}
	let srcPath = source.path;
	// Normalize path separators for the current OS.
	if (path.sep === '\\') {
		srcPath = srcPath.replace(/\//g, '\\');
	}
	const isAbs = path.isAbsolute(srcPath) || /^[a-zA-Z]:[\\/]/.test(srcPath);
	if (!isAbs) {
		// Try to resolve relative paths against workspace folders.
		const folders = vscode.workspace.workspaceFolders ?? [];
		for (const folder of folders) {
			const candidate = path.resolve(folder.uri.fsPath, srcPath);
			if (fs.existsSync(candidate)) {
				srcPath = candidate;
				break;
			}
		}
	}
	// Ignore disassembly or non-file sources we still couldn't resolve.
	if (!(path.isAbsolute(srcPath) || /^[a-zA-Z]:[\\/]/.test(srcPath))) {
		return false;
	}
	if (!fs.existsSync(srcPath)) {
		return false;
	}
	const line = typeof frame.line === 'number' && frame.line > 0 ? frame.line : 1;
	return autoRevealSourcePath(srcPath, line);
}

/** Returns true when the location is shown in an editor. */
async function autoRevealSourcePath(srcPath: string, line: number): Promise<boolean> {
	const norm = normalizePathForCompare(srcPath);
	// Skip only while the active editor still shows this location (the same stop is revealed by both
	// the focused stack frame and the adapter's sourceLocation event). A later stop at the same line
	// reveals again once the editor was closed, switched or the cursor moved.
	const active = vscode.window.activeTextEditor;
	if (
		lastAutoSourcePath === srcPath &&
		lastAutoSourceLine === line &&
		active?.document?.uri?.fsPath &&
		normalizePathForCompare(active.document.uri.fsPath) === norm &&
		active.selection.active.line === line - 1
	) {
		focusFileLog(vscode.debug.activeDebugSession, `reveal skipped (already shown) ${srcPath}:${line}`);
		return true;
	}
	try {
		const visible = vscode.window.visibleTextEditors.find((ed) => {
			const edPath = ed.document?.uri?.fsPath;
			return edPath ? normalizePathForCompare(edPath) === norm : false;
		});

		let editor: vscode.TextEditor;
		if (visible) {
			editor = await vscode.window.showTextDocument(visible.document, {
				viewColumn: visible.viewColumn,
				preview: isPreviewTab(visible.document.uri, visible.viewColumn),
				preserveFocus: false
			});
		} else {
			const uri = vscode.Uri.file(srcPath);
			const doc = await vscode.workspace.openTextDocument(uri);
			const viewColumn = preferredSourceViewColumn ?? vscode.ViewColumn.One;
			// Preview tab, like VS Code's own stop reveal: the next stop replaces it, so only the
			// current location stays open instead of a tab per stop.
			editor = await vscode.window.showTextDocument(doc, {
				viewColumn,
				preview: true,
				preserveFocus: false
			});
		}
		const pos = new vscode.Position(line - 1, 0);
		editor.selection = new vscode.Selection(pos, pos);
		editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
		lastAutoSourcePath = srcPath;
		lastAutoSourceLine = line;
		focusFileLog(vscode.debug.activeDebugSession, `revealed ${srcPath}:${line}`);
		return true;
	} catch (e) {
		agentOutput?.appendLine(`Failed to auto-open source file: ${String(e)}`);
		return false;
	}
}

/** Appends to %TEMP%/aurix-debugger-focus.log when the session's launch configuration has "debugUIFocus": true. */
function focusFileLog(session: vscode.DebugSession | undefined, message: string): void {
	if (session?.type !== 'aurixdbg' || session.configuration?.debugUIFocus !== true) {
		return;
	}
	try {
		const dir = process.env.TEMP || process.env.TMPDIR || '/tmp';
		fs.appendFileSync(path.join(dir, 'aurix-debugger-focus.log'), `${new Date().toISOString()} [extension] ${message}\n`);
	} catch {
		// diagnostics only
	}
}

function normalizePathForCompare(p: string): string {
	const resolved = path.resolve(p);
	return process.platform === 'win32' ? resolved.toLowerCase() : resolved;
}


export function deactivate() {
	void stopTCFAgent('extension deactivate');
	agentOutput?.dispose();
	agentOutput = undefined;
}
