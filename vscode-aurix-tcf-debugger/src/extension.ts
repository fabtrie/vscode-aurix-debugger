/*
 * Aurix TCF Debugger VSCode Extension
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
import { showDebugConfigPanel } from './debugConfigPanel';
import { LiveWatchCommandArgument, LiveWatchProvider } from './liveWatchView';
import { peripheralWatchEntryFromNode } from './peripheralLiveWatch';
import { getSvdOptionsForDevice } from './svdCatalog';
import { registerDebuggerActions } from './debuggerActions';
import { VsCodeBreakpointUi } from './vscodeBreakpointUi';

let tcfAgentProcess: ChildProcess | undefined;
let extensionContext: vscode.ExtensionContext;
let agentOutput: vscode.OutputChannel | undefined;
let flasherOutput: vscode.OutputChannel | undefined;
let activeAurixSessions = 0;
let startingAurixSessions = 0;
let pendingStopTimer: NodeJS.Timeout | undefined;
let pendingStopToken = 0;
let lastAgentHost: string | undefined;
let lastAgentPort: number | undefined;
let enforcingMemoryInspectorSettings = false;
const memoryInspectorExtensionId = 'eclipse-cdt.memory-inspector';
const openedDisassemblySessions = new Set<string>();
const aurixDebugSessions = new Set<vscode.DebugSession>();
let resolvedDisassemblyOpenCommand: string | undefined;
let instructionStepStatusItem: vscode.StatusBarItem | undefined;
const instructionStepStateKey = 'aurix-tcf.instructionSteppingEnabled';
let lastInstructionSteppingEnabled = false;
let liveWatchStatusItem: vscode.StatusBarItem | undefined;
let liveWatchProvider: LiveWatchProvider | undefined;
const liveWatchEnabledStateKey = 'aurix-tcf.liveWatch.enabled';
const liveWatchIntervalStateKey = 'aurix-tcf.liveWatch.intervalMs';
let lastLiveWatchEnabled = false;
let lastLiveWatchIntervalMs = 1000;
let preferredSourceViewColumn: vscode.ViewColumn | undefined;
let lastAutoSourcePath: string | undefined;
let lastAutoSourceLine: number | undefined;
const SVD_CACHE_FOLDER = 'svd-cache';
const DISASSEMBLY_OPEN_DELAY_MS = 1200;
const pendingDisassemblyOpenTimers = new Map<string, NodeJS.Timeout>();
const LIVE_WATCH_INTERVAL_OPTIONS = [200, 500, 1000, 2000] as const;

export function activate(context: vscode.ExtensionContext) {
	// This method is called when the extension is activated
	extensionContext = context;
	console.log('Aurix TCF debugging extension is activating...');
	agentOutput = vscode.window.createOutputChannel('AURIX TCF Agent');
	flasherOutput = vscode.window.createOutputChannel('AURIX Flasher');
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
			if (event.affectsConfiguration('aurix-tcf.disassembly.opcodeBytes')) {
				const opcodeBytes = getDisassemblyOpcodeBytes();
				for (const s of aurixDebugSessions) {
					void Promise.resolve(s.customRequest('aurix-tcf/setDisassemblyOptions', { opcodeBytes })).catch(() => undefined);
				}
			}
		})
	);

	// Commands (added): open the debug configuration webview panel
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.openDebugConfig', () => {
			void showDebugConfigPanel(context);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.pickSvd', async (args?: { targetDevice?: string }) => {
			void args;
			const options = await getSvdOptionsForDevice(context.extensionPath);
			const items: Array<vscode.QuickPickItem & { value?: string; browse?: boolean }> = [
				...options.map((option) => ({ label: option.label, description: option.value, value: option.value })),
				{ label: 'Browse local SVD...', description: 'Select any .svd file from disk', browse: true },
			];

			const picked = await vscode.window.showQuickPick(items, {
				title: 'Select SVD file',
				placeHolder: 'Pick SVD file',
				ignoreFocusOut: true,
			});
			if (!picked) {
				return undefined;
			}
			if (picked.browse) {
				const selected = await vscode.window.showOpenDialog({
					canSelectMany: false,
					openLabel: 'Select SVD',
					filters: {
						svdFiles: ['svd'],
						allFiles: ['*'],
					},
				});
				return selected?.[0]?.fsPath;
			}

			return picked.value;
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.resetTarget', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurix-tcf') {
				void vscode.window.showWarningMessage('No active AURIX™ TCF debug session.');
				return;
			}
			// The launch configuration decides: "resetSuspend": true halts at the reset vector,
			// otherwise the target runs after reset and is paused afterwards (pauseAll), since
			// some firmware only relaxes its watchdog after a startup check that runs a short
			// time after reset.
			await s.customRequest('aurix-tcf/reset', {});
		})
	);

	let flashing = false;
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.flashTarget', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurix-tcf') {
				void vscode.window.showWarningMessage('No active AURIX™ TCF debug session.');
				return;
			}
			if (flashing) {
				void vscode.window.showWarningMessage('AURIX™ TCF: flashing is already running.');
				return;
			}
			flashing = true;
			try {
				await vscode.window.withProgress(
					{ location: vscode.ProgressLocation.Notification, title: 'AURIX™ TCF: flashing target (see Debug Console)...' },
					async () => { await s.customRequest('aurix-tcf/flash', {}); }
				);
				// Same as the Reset button: the launch configuration decides whether the target halts or runs.
				await s.customRequest('aurix-tcf/reset', {});
				void vscode.window.showInformationMessage('AURIX™ TCF: flashing done.');
			} catch (e) {
				void vscode.window.showErrorMessage(`AURIX™ TCF: flashing failed: ${e instanceof Error ? e.message : String(e)}`);
			} finally {
				flashing = false;
			}
		})
	);

	registerDebuggerActions(context);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.showResetCapabilities', async () => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurix-tcf') {
				void vscode.window.showWarningMessage('No active AURIX™ TCF debug session.');
				return;
			}
			const result = await vscode.window.withProgress(
				{ location: vscode.ProgressLocation.Notification, title: 'Querying ContextReset.getCapabilities...' },
				() => s.customRequest('aurix-tcf/resetCapabilities')
			);
			const caps = Array.isArray(result?.capabilities) ? result.capabilities : [];
			if (result?.error) {
				void vscode.window.showErrorMessage(`ContextReset.getCapabilities failed: ${result.error}`);
			} else if (caps.length === 0) {
				void vscode.window.showInformationMessage('Agent reported no ContextReset reset types. See "TCF Agent" / debug console output.');
			} else {
				const summary = caps.map((c: { Type: string; Description?: string }) => c.Description ? `${c.Type} (${c.Description})` : c.Type).join(', ');
				void vscode.window.showInformationMessage(`Supported reset types: ${summary}`);
			}
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.toggleInstructionStepping', async () => {
			const enabled = !(await getInstructionSteppingEnabled());
			await setInstructionSteppingMode(enabled);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.disassemblyGoTo', async (target?: string) => {
			const s = vscode.debug.activeDebugSession;
			if (!s || s.type !== 'aurix-tcf') {
				void vscode.window.showWarningMessage('No active AURIX™ TCF debug session.');
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
		vscode.commands.registerCommand('aurix-tcf.toggleLiveWatch', async () => {
			const enabled = !(await getLiveWatchEnabled());
			await setLiveWatchMode(enabled);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.configureLiveWatch', async () => {
			const currentEnabled = await getLiveWatchEnabled();
			const currentInterval = await getLiveWatchIntervalMs();
			const items: Array<vscode.QuickPickItem & { enabled: boolean; intervalMs?: number; custom?: boolean }> = [
				{ label: 'Off', description: currentEnabled ? '' : 'Current', enabled: false },
				...LIVE_WATCH_INTERVAL_OPTIONS.map((intervalMs) => ({
					label: `${intervalMs} ms`,
					description: currentEnabled && currentInterval === intervalMs ? 'Current' : '',
					enabled: true,
					intervalMs,
				})),
				{
					label: 'Custom...',
					description: currentEnabled && !LIVE_WATCH_INTERVAL_OPTIONS.includes(currentInterval as any) ? `Current: ${currentInterval} ms` : '',
					enabled: true,
					custom: true,
				},
			];
			const picked = await vscode.window.showQuickPick(items, {
				title: 'Configure Live Watch',
				placeHolder: 'Select refresh interval for running global watch values (minimum 200ms+)',
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
				await setLiveWatchMode(true, Number(customValue.trim()));
				return;
			}
			await setLiveWatchMode(picked.enabled, picked.intervalMs);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchAddExpression', async () => {
			await liveWatchProvider?.addExpression(await pickLiveWatchExpression());
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchRemoveExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.removeExpression(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchSetValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setValue(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchPauseExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setExpressionPaused(item, true);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchResumeExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.setExpressionPaused(item, false);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchAddPeripheral', async (node?: unknown) => {
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
		vscode.commands.registerCommand('aurix-tcf.liveWatchEditExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.editExpression(item);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchCopyValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.copyValue(item);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchCopyExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatchProvider?.copyExpression(item);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchDisableRefresh', async () => {
			await setLiveWatchMode(false);
		}),
		vscode.commands.registerCommand('aurix-tcf.liveWatchEnableRefresh', async () => {
			await setLiveWatchMode(true);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchClearExpressions', async () => {
			await liveWatchProvider?.clearExpressions();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.liveWatchRefresh', async () => {
			await liveWatchProvider?.refreshNow();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.enableInstructionStepping', async () => {
			await setInstructionSteppingMode(true);
		}),
		vscode.commands.registerCommand('aurix-tcf.disableInstructionStepping', async () => {
			await setInstructionSteppingMode(false);
		})
	);

	instructionStepStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
	instructionStepStatusItem.command = 'aurix-tcf.toggleInstructionStepping';
	instructionStepStatusItem.tooltip = 'Toggle instruction-level stepping';
	context.subscriptions.push(instructionStepStatusItem);
	liveWatchStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
	liveWatchStatusItem.command = 'aurix-tcf.configureLiveWatch';
	liveWatchStatusItem.tooltip = 'Configure live watch refresh for running global variables';
	context.subscriptions.push(liveWatchStatusItem);
	context.subscriptions.push(vscode.window.registerWebviewViewProvider(LiveWatchProvider.viewType, liveWatchProvider));
	// Like the Watch view, the Live Watch updates on every break, also for paused expressions and when
	// the periodic refresh is off. The adapter's stopped event also covers cores stopping without focus.
	context.subscriptions.push(
		vscode.debug.registerDebugAdapterTrackerFactory('aurix-tcf', {
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
		vscode.debug.registerDebugConfigurationProvider('aurix-tcf', new AurixTCFConfigurationProvider())
	);

	// Register debug adapter factory
	context.subscriptions.push(
		vscode.debug.registerDebugAdapterDescriptorFactory('aurix-tcf', new AurixTCFDebugAdapterFactory())
	);

	// Register timestamp command (useful for recording TCF messages with timestamps)
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.timestamp', () => {
			return new Date().toISOString().replaceAll(":", "-");
		})
	);

	// Ensure we clean up the agent when the debug session is terminated.
	context.subscriptions.push(
		vscode.debug.onDidStartDebugSession((session: vscode.DebugSession) => {
			if (session.type === 'aurix-tcf') {
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
			if (!session || session.type !== 'aurix-tcf') {
				return;
			}
			// The adapter needs the focused thread to tell the toolbar pause button (which pauses the
			// focused thread) from a core's pause button in the Call Stack view.
			const focused = item && item.session.id === session.id ? item.threadId : undefined;
			focusFileLog(session, `activeStackItem thread=${String(focused)} frame=${String((item as vscode.DebugStackFrame | undefined)?.frameId)}`);
			void Promise.resolve(session.customRequest('aurix-tcf/activeThread', { threadId: focused })).catch(() => undefined);
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
			if (e.session.type !== 'aurix-tcf') {
				return;
			}
			if (e.event !== 'aurix-tcf/sourceLocation') {
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
			if (session.type !== 'aurix-tcf') {
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
				updateLiveWatchStatus(lastLiveWatchEnabled, lastLiveWatchIntervalMs, false);
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

	void vscode.window.showInformationMessage('Aurix TCF debugging extension is enabled.');
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

		// Pre-launch validation for Flash Hex (PPU / CDSP).
		// AurixFlasher returns exit code 0 even when no HEX file is provided,
		// so explicit validation is required to prevent silent failures.
		{
			const missingHexFiles: string[] = [];
			if (config.ppuFlashHex === true) {
				const ppuElf = resolveConfigPath(session, config.ppuElfPath);
				if (!ppuElf) {
					missingHexFiles.push('PPU: Flash Hex is enabled but no ELF file is specified.');
				} else {
					const hexPath = ppuElf.replace(/\.elf$/i, '.hex');
					if (!fs.existsSync(hexPath)) {
						missingHexFiles.push(`PPU: HEX file not found: ${hexPath}`);
					}
				}
			}
			if (config.cdspFlashHex === true) {
				const cdspPaths: string[] = Array.isArray(config.cdspElfPaths) ? config.cdspElfPaths : [];
				let anyCdsp = false;
				for (let i = 0; i < cdspPaths.length; i++) {
					const elfPath = resolveConfigPath(session, cdspPaths[i]);
					if (elfPath) {
						anyCdsp = true;
						const hexPath = elfPath.replace(/\.elf$/i, '.hex');
						if (!fs.existsSync(hexPath)) {
							missingHexFiles.push(`CDSP${i}: HEX file not found: ${hexPath}`);
						}
					}
				}
				if (!anyCdsp) {
					missingHexFiles.push('CDSP: Flash Hex is enabled but no ELF files are specified.');
				}
			}
			if (missingHexFiles.length > 0) {
				void vscode.window.showErrorMessage(
					`Flash Hex validation failed:\n${missingHexFiles.join('\n')}`,
					{ modal: true },
				);
				throw new Error('Flash Hex validation failed');
			}
		}

		// Flash firmware via AurixFlasher (single invocation, matching Eclipse launcher).
		// TriCore ELF uses -elf; PPU/CDSP HEX files use -hex.
		// aurix-dbg programs the flash itself over TAS (Tcl 'program' after connect).
		if (config.enableFlashing === true && config.agentFlavor !== 'aurix-dbg') {
			const resolvedElf = resolveConfigPath(session, config.program);
			if (!resolvedElf) {
				void vscode.window.showErrorMessage('Enable flashing is set but no program/ELF path is configured.');
				throw new Error('enableFlashing requires a valid program path');
			}

			// Collect all HEX files for PPU/CDSP.
			const hexFiles: string[] = [];
			if (config.ppuFlashHex === true) {
				const ppuElf = resolveConfigPath(session, config.ppuElfPath);
				if (ppuElf) {
					hexFiles.push(ppuElf.replace(/\.elf$/i, '.hex'));
				}
			}
			if (config.cdspFlashHex === true) {
				const cdspPaths: string[] = Array.isArray(config.cdspElfPaths) ? config.cdspElfPaths : [];
				for (let i = 0; i < cdspPaths.length; i++) {
					const elfPath = resolveConfigPath(session, cdspPaths[i]);
					if (elfPath) {
						hexFiles.push(elfPath.replace(/\.elf$/i, '.hex'));
					}
				}
			}

			await runAurixFlasher(
				resolvedElf,
				typeof config.targetDevice === 'string' ? config.targetDevice : undefined,
				hexFiles,
			);
		}

		// Auto-start TCF agent if requested
		if (config.autoStartAgent && config.agentFlavor === 'aurix-dbg') {
			await startAurixDbg(session, config);
		} else if (config.autoStartAgent) {
			// Preflight: validate targetDevice early so typos fail fast.
			if (typeof config.targetDevice === 'string' && config.targetDevice.trim() !== '') {
				try {
					await validateAgentTargetDevice(config.targetDevice);
				} catch (e) {
					const msg = e instanceof Error ? e.message : String(e);
					void vscode.window.showErrorMessage(`Invalid targetDevice for agent: ${msg}`);
					throw e;
				}
			}

			// Use configured agent path, or fallback to bundled agent
			const configuredPath = resolveConfigPath(session, config.agentPath || vscode.workspace.getConfiguration("aurix-tcf").get('agentPath'));
			const agentPath = configuredPath || path.join(extensionContext.extensionPath, 'bin', 'aurix-debugger.exe');
			
			// Build agent arguments based on Eclipse log format
			const agentArgs: string[] = [];
			const userArgs: string[] = [];
			
			// -s TCP:host:port (server mode)
			agentArgs.push('-s', `TCP:${config.host || '127.0.0.1'}:${config.port || 1534}`);
			
			// -p device_name (target device)
			if (config.targetDevice) {
				agentArgs.push('-p', config.targetDevice);
			}
			
			// -e entry_function
			if (config.entryFunction) {
				agentArgs.push('-e', config.entryFunction);
			}
			
			// -b core_number
			if (config.core !== undefined) {
				agentArgs.push('-b', String(config.core));
			}
			
			// -r svd_file_path
			if (config.svdFile) {
				agentArgs.push('-r', config.svdFile);
			}
			
			// -K workspace_path
			if (config.workspace) {
				agentArgs.push('-K', config.workspace);
			}
			
			// -f and -G flags
			if (config.flashDownload !== false) {
				agentArgs.push('-f');
			}

			// Collect user-provided args first so they can override auto-generated flags.
			// This is important for multi-core selection (-A) where the launcher may emit
			// a mask that does not correspond 1:1 with the ELF-path fields in this extension.
			if (typeof config.additionalAgentOptions === 'string' && config.additionalAgentOptions.trim() !== '') {
				userArgs.push(...splitCommandLine(config.additionalAgentOptions));
			}
			if (config.agentArgs && Array.isArray(config.agentArgs)) {
				userArgs.push(...config.agentArgs);
			}

			// Auto-generate -A core selection mask from PPU/CDSP/SCR ELF paths,
			// unless the user already provided -A in additionalAgentOptions or agentArgs.
			if (!userProvidedAParameter(userArgs)) {
				const mask = buildCoreSelectionMask(config);
				if (mask) {
					agentArgs.push('-A', mask);
				}
			}

			agentArgs.push(...userArgs);
			
			const host = config.host || '127.0.0.1';
			const port = config.port || 1534;

			// Keep this simple: if an agent is already running, reuse it.
			// This makes VS Code Restart stable (no kill+start churn).
			let reused = false;
			if (tcfAgentProcess && tcfAgentProcess.exitCode === null) {
				agentOutput?.appendLine('Reusing already running TCF agent for new session.');
				try {
					await waitForTcpPort(host, port, 15_000);
					reused = true;
				} catch (e) {
					agentOutput?.appendLine(`Running TCF agent is not reachable (${e instanceof Error ? e.message : String(e)}); restarting it.`);
				}
			}
			if (!reused) {
				await startTCFAgent(agentPath, agentArgs, host, port);
			}
		}

		// Enable path mapper based on workspace trust and configuration
		const pathMapperConfigDefault = vscode.workspace.getConfiguration("aurix-tcf").inspect("executePathMapper")?.defaultValue ?? true;
		const pathMapperConfig = vscode.workspace.getConfiguration("aurix-tcf").get('executePathMapper', pathMapperConfigDefault);

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


function parseCsvLine(line: string): string[] {
	// Minimal CSV parser supporting quoted fields.
	const fields: string[] = [];
	let current = '';
	let inQuotes = false;
	for (let i = 0; i < line.length; i++) {
		const ch = line[i];
		if (ch === '"') {
			if (inQuotes && line[i + 1] === '"') {
				current += '"';
				i++;
				continue;
			}
			inQuotes = !inQuotes;
			continue;
		}
		if (ch === ',' && !inQuotes) {
			fields.push(current.trim());
			current = '';
			continue;
		}
		current += ch;
	}
	fields.push(current.trim());
	return fields;
}

type LsmcddDevice = {
	id: string;
	name: string;
	family: string;
};

async function listLsmcddDevices(): Promise<LsmcddDevice[]> {
	const exePath = path.join(extensionContext.extensionPath, 'bin', 'lsmcdd.exe');
	if (!fs.existsSync(exePath)) {
		throw new Error(`lsmcdd.exe not found: ${exePath}`);
	}
	const output = await new Promise<string>((resolve, reject) => {
		execFile(
			exePath,
			{ cwd: path.dirname(exePath), windowsHide: true, maxBuffer: 5 * 1024 * 1024, timeout: 10_000 },
			(err, stdout, stderr) => {
			const combined = (stdout && stdout.trim().length > 0) ? stdout : (stderr ?? '');
			if (err && (!combined || combined.trim().length === 0)) {
				reject(new Error((stderr || err.message || 'Unknown error').trim()));
				return;
			}
			resolve(combined ?? '');
		}
		);
	});
	const devices: LsmcddDevice[] = [];
	for (const rawLine of output.split(/\r?\n/)) {
		const line = rawLine.trim();
		if (!line) {
			continue;
		}
		const fields = parseCsvLine(line);
		const id = (fields[0] ?? '').trim();
		const name = (fields[1] ?? '').trim();
		const family = (fields[3] ?? '').trim();
		if (id !== '' && name !== '') {
			devices.push({ id, name, family });
		}
	}
	return devices;
}

async function validateAgentTargetDevice(targetDevice: string): Promise<void> {
	const raw = targetDevice.trim();
	if (raw === '') {
		return;
	}
	if (/^\d+$/.test(raw)) {
		throw new Error(`targetDevice must be a device name (not a numeric id).`);
	}
	try {
		const devices = await listLsmcddDevices();
		if (devices.length === 0) {
			throw new Error('No devices detected by lsmcdd.exe');
		}
		const rawLower = raw.toLowerCase();
		const ok = devices.some((d) => (d.name ?? '').trim() !== '' && d.name.toLowerCase() === rawLower);
		if (!ok) {
			const names = devices.map((d) => d.name).join('; ');
			throw new Error(`Device '${raw}' not found. Available: ${names}`);
		}
	} catch (e) {
		// If we cannot validate (tool missing/timeout), don't block debugging.
		// But if we *did* get a device list and it doesn't match, we throw above.
		const msg = e instanceof Error ? e.message : String(e);
		agentOutput?.appendLine(`Warning: could not validate targetDevice via lsmcdd.exe: ${msg}`);
		// Re-throw only for definite mismatch errors (which include 'Available:').
		if (typeof msg === 'string' && msg.includes('Available:')) {
			throw e;
		}
	}
}

async function resolveFlasherDeviceId(device: string | undefined): Promise<string> {
	const raw = (device ?? '').trim();
	if (raw !== '' && /^\d+$/.test(raw)) {
		throw new Error(`targetDevice must be a device name (not a numeric id).`);
	}

	const devices = await listLsmcddDevices();
	if (devices.length === 0) {
		throw new Error('No devices detected by lsmcdd.exe');
	}

	// If the user did not specify a device and there is exactly one, pick it.
	if (raw === '' && devices.length === 1) {
		return devices[0].id;
	}
	if (raw === '') {
		const names = devices.map((d) => d.name).join('; ');
		throw new Error(`Multiple devices detected; please set targetDevice. Available: ${names}`);
	}

	const rawLower = raw.toLowerCase();
	const exactName = devices.find((d) => d.name === raw) ?? devices.find((d) => d.name.toLowerCase() === rawLower);
	if (exactName) {
		return exactName.id;
	}

	// Accept matching by family (e.g., TC4Dx / TC37x) as a fallback, but only if unique.
	const familyMatches = devices.filter((d) => (d.family ?? '').trim() !== '' && d.family.toLowerCase() === rawLower);
	if (familyMatches.length === 1) {
		return familyMatches[0].id;
	}
	if (familyMatches.length > 1) {
		const names = familyMatches.map((d) => d.name).join('; ');
		throw new Error(`Device '${raw}' matches multiple devices (by family). Matches: ${names}`);
	}

	const names = devices.map((d) => d.name).join('; ');
	throw new Error(`Device '${raw}' not found. Available: ${names}`);
}

async function runAurixFlasher(elfPath: string, targetDevice: string | undefined, hexFiles: string[] = []): Promise<void> {
	// Single invocation matching Eclipse launcher:
	//   aurixflasher.exe -gui off -elf <tricore.elf> [-hex <ppu.hex> ...] -start off -ver on -id <deviceId>
	// If first attempt fails, retry with -erase all.
	const flasherExe = path.join(extensionContext.extensionPath, 'bin', 'aurixflasher', 'aurixflasher.exe');
	const flasherCwd = path.dirname(flasherExe);

	if (!fs.existsSync(flasherExe)) {
		void vscode.window.showErrorMessage(`aurixflasher.exe not found: ${flasherExe}`);
		throw new Error('aurixflasher.exe not found');
	}
	if (!fs.existsSync(elfPath)) {
		void vscode.window.showErrorMessage(`ELF file not found: ${elfPath}`);
		throw new Error('ELF file not found');
	}
	for (const hex of hexFiles) {
		if (!fs.existsSync(hex)) {
			void vscode.window.showErrorMessage(`HEX file not found: ${hex}`);
			throw new Error(`HEX file not found: ${hex}`);
		}
	}

	// Show output early so users see progress while we run lsmcdd.exe.
	flasherOutput?.show(true);
	flasherOutput?.appendLine(`Flashing ELF: ${elfPath}`);
	for (const hex of hexFiles) {
		flasherOutput?.appendLine(`Flashing HEX: ${hex}`);
	}
	flasherOutput?.appendLine(`Resolving device id via lsmcdd.exe (targetDevice='${targetDevice ?? ''}')...`);

	let deviceId: string;
	try {
		deviceId = await resolveFlasherDeviceId(targetDevice);
	} catch (e) {
		const msg = e instanceof Error ? e.message : String(e);
		flasherOutput?.appendLine(`Failed to resolve device id: ${msg}`);
		void vscode.window.showErrorMessage(`Flashing failed: cannot resolve device id from lsmcdd.exe output. ${msg}`);
		throw e;
	}

	// Build command: -gui off -elf <path> [-hex <path>]... -start off -ver on -id <deviceId>
	const baseArgs: string[] = [
		'-gui', 'off',
		'-elf', elfPath,
	];
	for (const hex of hexFiles) {
		baseArgs.push('-hex', hex);
	}
	baseArgs.push('-start', 'off', '-ver', 'on', '-id', deviceId);

	flasherOutput?.appendLine(`Device: ${targetDevice ?? ''} (id ${deviceId})`);
	flasherOutput?.appendLine(`Command: ${flasherExe} ${baseArgs.join(' ')}`);

	const runFlasher = (args: string[]): Promise<{ exitCode: number | null; stdout: string; stderr: string }> => {
		return new Promise((resolve) => {
			execFile(flasherExe, args, { cwd: flasherCwd, windowsHide: true, maxBuffer: 20 * 1024 * 1024 }, (err, stdout, stderr) => {
				const exitCode = err && 'code' in err ? (err as any).code as number : (err ? 1 : 0);
				resolve({ exitCode, stdout: String(stdout ?? ''), stderr: String(stderr ?? '') });
			});
		});
	};

	// First attempt.
	let result = await runFlasher(baseArgs);
	if (result.stdout) { flasherOutput?.appendLine(result.stdout.trimEnd()); }
	if (result.stderr) { flasherOutput?.appendLine(result.stderr.trimEnd()); }

	if (result.exitCode !== 0) {
		// Retry with -erase all (matching Eclipse launcher behaviour).
		flasherOutput?.appendLine('Flashing failed, retrying with -erase all...');
		const retryArgs = [...baseArgs, '-erase', 'all'];
		flasherOutput?.appendLine(`Command: ${flasherExe} ${retryArgs.join(' ')}`);

		result = await runFlasher(retryArgs);
		if (result.stdout) { flasherOutput?.appendLine(result.stdout.trimEnd()); }
		if (result.stderr) { flasherOutput?.appendLine(result.stderr.trimEnd()); }

		if (result.exitCode !== 0) {
			const msg = `Flashing failed even after erasing. Exit code: ${result.exitCode}`;
			flasherOutput?.appendLine(msg);
			void vscode.window.showErrorMessage(msg);
			throw new Error(msg);
		}
	}

	flasherOutput?.appendLine('Flashing completed successfully.');
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

function buildCoreSelectionMask(config: any): string | undefined {
	const hasPath = (v: any) => typeof v === 'string' && v.trim() !== '';
	const chars = new Array(14).fill('-');
	if (hasPath(config.ppuElfPath)) {
		chars[0] = 'p';
	}
	const cdsp = Array.isArray(config.cdspElfPaths) ? config.cdspElfPaths : [];
	for (let i = 0; i < 12; i++) {
		if (hasPath(cdsp[i])) {
			chars[i + 1] = 'c';
		}
	}
	if (hasPath(config.scrElfPath)) {
		chars[13] = 's';
	}
	const mask = chars.join('');
	return /[^-]/.test(mask) ? mask : undefined;
}

function userProvidedAParameter(args: string[]): boolean {
	for (const a of args) {
		if (a === '-A') {
			return true;
		}
		// Allow combined form: -Apcc-----
		if (a.startsWith('-A') && a.length > 2) {
			return true;
		}
	}
	return false;
}

function resolveConfigPathFromFolder(folder: vscode.WorkspaceFolder | undefined, p: unknown): string | undefined {
	if (typeof p !== 'string' || p.trim() === '') {
		return undefined;
	}
	let out = p;
	const ws = folder?.uri.fsPath || vscode.workspace.workspaceFolders?.[0]?.uri.fsPath || '';
	if (ws) {
		out = out.replaceAll('${workspaceFolder}', ws);
		out = out.replaceAll('${workspaceRoot}', ws);
	}
	out = out.replace(/\//g, path.sep);
	if (!path.isAbsolute(out) && ws) {
		out = path.resolve(ws, out);
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
			title: 'AURIX™ TCF: preparing SVD definitions (first run)',
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

async function resolveUsableSvdPath(
	folder: vscode.WorkspaceFolder | undefined,
	value: string | undefined,
): Promise<string | undefined> {
	if (!value || value.trim() === '') {
		return undefined;
	}

	const resolved = resolveConfigPathFromFolder(folder, value) ?? value;
	const lower = resolved.toLowerCase();
	if (lower.endsWith('.svd.gz')) {
		return await materializeCompressedSvd(resolved);
	}
	return resolved;
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
				type: 'aurix-tcf',
				request: 'launch',
				name: 'Debug Aurix Program',
				workspace: '${workspaceFolder}',
				program: '${workspaceFolder}/build/program.elf',
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
			void vscode.window.showErrorMessage('AURIX™ TCF Debugger: attach is not supported. Use a launch configuration (via the GUI panel).');
			return undefined;
		}

		// If launch.json is missing or empty
		if (!config.type && !config.request && !config.name) {
			const editor = vscode.window.activeTextEditor;
			if (editor && (editor.document.languageId === 'cpp' || editor.document.languageId === 'c')) {
				config.type = 'aurix-tcf';
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
		if (typeof (config as any).liveWatchEnabled !== 'boolean') {
			(config as any).liveWatchEnabled = await getLiveWatchEnabled();
		}
		if (typeof (config as any).liveWatchIntervalMs !== 'number') {
			(config as any).liveWatchIntervalMs = await getLiveWatchIntervalMs();
		}

		// Console logging toggle: enableConsoleLog maps to debugTCFMessages.
		if (typeof (config as any).enableConsoleLog === 'boolean') {
			(config as any).debugTCFMessages = (config as any).enableConsoleLog;
		}

		const asNonEmptyString = (value: any): string | undefined => {
			return typeof value === 'string' && value.trim() !== '' ? value : undefined;
		};
		const looksLikeFilePath = (value: string): boolean => {
			const v = value.trim();
			if (v === '' || v.includes('::')) {
				return false;
			}
			return v.toLowerCase().endsWith('.svd') || v.toLowerCase().endsWith('.svd.gz') || v.includes('${') || /[\\/]/.test(v);
		};

		const svdFile = asNonEmptyString((config as any).svdFile);
		const svdPath = asNonEmptyString((config as any).svdPath);
		const definitionPath = asNonEmptyString((config as any).definitionPath);

		const effectiveSvdFile = asNonEmptyString((config as any).svdFile);

		if (!svdPath && effectiveSvdFile) {
			(config as any).svdPath = effectiveSvdFile;
		}
		if (!definitionPath && effectiveSvdFile) {
			(config as any).definitionPath = effectiveSvdFile;
		}
		if (!effectiveSvdFile) {
			const candidate = svdPath || definitionPath;
			if (candidate && looksLikeFilePath(candidate)) {
				(config as any).svdFile = candidate;
			}
		}

		const usableSvd = await resolveUsableSvdPath(folder, asNonEmptyString((config as any).svdFile));
		if (usableSvd) {
			(config as any).svdFile = usableSvd;
			(config as any).svdPath = usableSvd;
			(config as any).definitionPath = usableSvd;
		}

		return config;
	}
}

/**
 * Starts aurix-dbg (OpenOCD-compatible debug server over Infineon TAS) with
 * OpenOCD-style arguments built from the launch configuration:
 * -s <searchDirs> -f <configFiles> -c <configCommands>.
 */
async function startAurixDbg(session: vscode.DebugSession, config: vscode.DebugConfiguration): Promise<void> {
	const agentPath = resolveConfigPath(session, config.agentPath || vscode.workspace.getConfiguration('aurix-tcf').get('agentPath'));
	if (!agentPath) {
		void vscode.window.showErrorMessage('agentFlavor "aurix-dbg" requires "agentPath" (path to aurix-dbg.exe).');
		throw new Error('aurix-dbg agentPath not configured');
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

	agentOutput?.show(true);
	agentOutput?.appendLine(`Starting TCF agent: ${agentPath}`);
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
			reject(new Error(`Failed to start TCF agent '${agentPath}': ${err.message}`));
		});
	});
	spawnFailed.catch(() => { /* surfaced via Promise.race below */ });

	// 'close', not 'exit': 'exit' can fire before the last stdout/stderr chunks have been
	// delivered, and those chunks are the failure reason.
	const closed = new Promise<number | null>((resolve) => proc.once('close', (code) => resolve(code)));
	const exitedEarly = closed.then((code): never => {
		throw new Error(`TCF agent exited with code ${code} before listening on ${host}:${port}`);
	});
	exitedEarly.catch(() => { /* surfaced via Promise.race below */ });

	proc.on('exit', (code, signal) => {
		agentOutput?.appendLine(`TCF agent exited with code ${code}${signal ? ` (signal ${signal})` : ''}`);
		if (tcfAgentProcess === proc) {
			tcfAgentProcess = undefined;
		}
	});

	proc.on('error', (err) => {
		agentOutput?.appendLine(`Failed to start TCF agent: ${err.message}`);
	});

	// Avoid a race where the debug adapter tries to connect before the agent has
	// started listening on the requested TCP port.
	agentOutput?.appendLine(`Waiting for TCF agent to listen on ${host}:${port}...`);
	try {
		await Promise.race([waitForTcpPort(host, port, 15_000), spawnFailed, exitedEarly]);
	} catch (e) {
		// If the process is already gone, let its streams drain so the reported tail is
		// complete (waitForTcpPort can reject on 'exit', i.e. before 'close').
		if (proc.exitCode !== null || proc.signalCode !== null) {
			await Promise.race([closed, new Promise((resolve) => setTimeout(resolve, 500))]);
		}
		const message = `${e instanceof Error ? e.message : String(e)}${formatAgentOutputTail(recentOutput)}`;
		agentOutput?.appendLine(`TCF agent did not start listening: ${message}`);
		agentOutput?.show(true);
		// Don't leave a hung agent behind: it keeps the probe locked and later launches would reuse it.
		await stopTCFAgent('startup failed');
		throw new Error(message);
	}
	agentOutput?.appendLine(`TCF agent is listening on ${host}:${port}`);
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

	agentOutput?.appendLine(`Stopping TCF agent (pid ${pid}) - ${reason}`);

	try {
		if (process.platform === 'win32') {
			// Kill the whole process tree; aurix-debugger.exe may spawn helper processes.
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
		agentOutput?.appendLine(`Failed to stop TCF agent cleanly: ${String(e)}`);
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
				agentOutput?.appendLine('Port still open after taskkill by PID; forcing taskkill by image name aurix-debugger.exe');
				try {
					await new Promise<void>((resolve, reject) => {
						execFile('taskkill', ['/IM', 'aurix-debugger.exe', '/T', '/F'], { windowsHide: true }, (err, stdout, stderr) => {
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
					agentOutput?.appendLine(`Failed to taskkill aurix-debugger.exe by image name: ${String(e)}`);
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
				reject(new Error('TCF agent process is not running'));
				return;
			}
			if (tcfAgentProcess.exitCode !== null) {
				reject(new Error(`TCF agent exited early (code ${tcfAgentProcess.exitCode})`));
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
	const desiredDebugger = 'aurix-tcf';
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
	const alive = !!activeSession && activeSession.id === session.id && activeSession.type === 'aurix-tcf';
	if (!alive || openedDisassemblySessions.has(session.id)) {
		return;
	}
	openedDisassemblySessions.add(session.id);
	void openDisassemblyEditorLayout(session);
}

async function openDisassemblyEditorLayout(session: vscode.DebugSession): Promise<void> {
	if (session.type !== 'aurix-tcf') {
		return;
	}
	const cfg = vscode.workspace.getConfiguration('aurix-tcf');
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
		symbolsRequest: 'aurix-tcf/disassemblySymbols',
	});
}

function pickLiveWatchExpression(): Promise<string | undefined> {
	const session = vscode.debug.activeDebugSession;
	return pickSymbolOrText({
		title: 'Add Live Watch Expression',
		placeholder: 'Pick a variable or type an expression, e.g. counter_6 or *(uint32_t*)0x70000044',
		typedDescription: 'Add expression',
		session: session?.type === 'aurix-tcf' ? session : undefined,
		symbolsRequest: 'aurix-tcf/variableSymbols',
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
		const result = await session.customRequest('aurix-tcf/disassemblyGoTo', { target });
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
	const cfg = vscode.workspace.getConfiguration('aurix-tcf');
	return cfg.get<boolean>('instructionSteppingDefault', false);
}

function getDisassemblyOpcodeBytes(): DisassemblyOpcodeBytes {
	const value = vscode.workspace.getConfiguration('aurix-tcf').get('disassembly.opcodeBytes');
	return isDisassemblyOpcodeBytes(value) ? value : 'left';
}

function normalizeLiveWatchIntervalMs(value: unknown): number {
	const numeric = typeof value === 'number' ? value : Number(value);
	if (!Number.isFinite(numeric)) {
		return 1000;
	}
	return Math.min(10_000, Math.max(200, Math.round(numeric)));
}

async function getLiveWatchEnabled(): Promise<boolean> {
	const stored = extensionContext.workspaceState.get<boolean>(liveWatchEnabledStateKey);
	if (typeof stored === 'boolean') {
		return stored;
	}
	const cfg = vscode.workspace.getConfiguration('aurix-tcf');
	return cfg.get<boolean>('liveWatch.enabled', false);
}

async function getLiveWatchIntervalMs(): Promise<number> {
	const stored = extensionContext.workspaceState.get<number>(liveWatchIntervalStateKey);
	if (typeof stored === 'number' && Number.isFinite(stored)) {
		return normalizeLiveWatchIntervalMs(stored);
	}
	const cfg = vscode.workspace.getConfiguration('aurix-tcf');
	return normalizeLiveWatchIntervalMs(cfg.get<number>('liveWatch.intervalMs', 1000));
}

async function setLiveWatchEnabled(value: boolean): Promise<void> {
	lastLiveWatchEnabled = value;
	await extensionContext.workspaceState.update(liveWatchEnabledStateKey, value);
}

async function setLiveWatchInterval(value: number): Promise<void> {
	lastLiveWatchIntervalMs = normalizeLiveWatchIntervalMs(value);
	await extensionContext.workspaceState.update(liveWatchIntervalStateKey, lastLiveWatchIntervalMs);
}

async function setLiveWatchMode(enabled: boolean, intervalMs?: number, session?: vscode.DebugSession): Promise<void> {
	const nextIntervalMs = normalizeLiveWatchIntervalMs(intervalMs ?? await getLiveWatchIntervalMs());
	await setLiveWatchEnabled(enabled);
	await setLiveWatchInterval(nextIntervalMs);
	const targetSession = session ?? vscode.debug.activeDebugSession;
	if (targetSession && targetSession.type === 'aurix-tcf') {
		try {
			await targetSession.customRequest('aurix-tcf/setLiveWatch', { enabled, intervalMs: nextIntervalMs });
		} catch (e) {
			agentOutput?.appendLine(`Failed to set live watch: ${String(e)}`);
		}
	}
	updateLiveWatchStatus(enabled, nextIntervalMs, true);
	liveWatchProvider?.refreshScheduling(enabled, nextIntervalMs);
	if (enabled) {
		void liveWatchProvider?.refreshNow();
	}
}

function updateLiveWatchStatus(enabled: boolean, intervalMs: number, visible: boolean): void {
	lastLiveWatchEnabled = enabled;
	lastLiveWatchIntervalMs = normalizeLiveWatchIntervalMs(intervalMs);
	void vscode.commands.executeCommand('setContext', 'aurix-tcf.liveWatchEnabled', enabled);
	if (!liveWatchStatusItem) {
		return;
	}
	liveWatchStatusItem.text = enabled ? `Live Watch: ${lastLiveWatchIntervalMs}ms` : 'Live Watch: OFF';
	liveWatchStatusItem.tooltip = enabled
		? `Live watch is enabled for running global watch expressions (${lastLiveWatchIntervalMs} ms). Click to change.`
		: 'Live watch is disabled. Click to enable or change interval.';
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
	if (targetSession && targetSession.type === 'aurix-tcf') {
		try {
			await targetSession.customRequest('aurix-tcf/setInstructionStepping', { enabled });
		} catch (e) {
			agentOutput?.appendLine(`Failed to set instruction stepping: ${String(e)}`);
		}
	}
	updateInstructionSteppingStatus(enabled, true);
}

function updateInstructionSteppingStatus(enabled: boolean, visible: boolean): void {
	lastInstructionSteppingEnabled = enabled;
	void vscode.commands.executeCommand('setContext', 'aurix-tcf.instructionStepping', enabled);
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
	if (session.type !== 'aurix-tcf') {
		return;
	}
	const enabled = await getInstructionSteppingEnabled();
	await setInstructionSteppingMode(enabled, session);
}

async function syncLiveWatchOnSessionStart(session: vscode.DebugSession): Promise<void> {
	if (session.type !== 'aurix-tcf') {
		return;
	}
	const sessionConfig = session.configuration as { liveWatchEnabled?: boolean; liveWatchIntervalMs?: number } | undefined;
	const enabled = typeof sessionConfig?.liveWatchEnabled === 'boolean'
		? sessionConfig.liveWatchEnabled
		: await getLiveWatchEnabled();
	const intervalMs = sessionConfig?.liveWatchIntervalMs !== undefined
		? normalizeLiveWatchIntervalMs(sessionConfig.liveWatchIntervalMs)
		: await getLiveWatchIntervalMs();
	await setLiveWatchMode(enabled, intervalMs, session);
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

/** Appends to %TEMP%/aurix-tcf-focus.log when the session's launch configuration has "debugUIFocus": true. */
function focusFileLog(session: vscode.DebugSession | undefined, message: string): void {
	if (session?.type !== 'aurix-tcf' || session.configuration?.debugUIFocus !== true) {
		return;
	}
	try {
		const dir = process.env.TEMP || process.env.TMPDIR || '/tmp';
		fs.appendFileSync(path.join(dir, 'aurix-tcf-focus.log'), `${new Date().toISOString()} [extension] ${message}\n`);
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
	flasherOutput?.dispose();
	flasherOutput = undefined;
}
