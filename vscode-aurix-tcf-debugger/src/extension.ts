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
import { AurixTCFDebugSession, DisassemblyOpcodeBytes, isDisassemblyOpcodeBytes } from './debugProvider';
import { LiveWatchCommandArgument } from './liveWatchView';
import { LiveWatchManager, pickIntervalMs } from './liveWatchManager';
import { OsObjectsPanel } from './osObjectsPanel';
import { UcbPanel } from './ucbPanel';
import { OsProfilePanel } from './osProfilePanel';
import { registerTraceStatus, TracePanel } from './tracePanel';
import { registerReverseUi, reverseStatus, reverseStopped, traceReplay } from './reverseUi';
import { DebugLayout, focusEditorGroup } from './debugLayout';
import { describeOsError } from './tcf/osaware';
import {
	notifyLiveUpdateBreak,
	setLiveUpdateIntervalMs,
	setLiveUpdatePauseWhileHalted,
	setLiveUpdateSessionActive,
	setLiveUpdateTargetHalted,
} from './liveUpdate';
import { OsViewProvider } from './osView';
import { peripheralWatchEntryFromNode } from './peripheralLiveWatch';
import { findSvdForDevice } from './svdCatalog';
import { PeripheralOptionsList } from './peripheralOptions';
import { registerDebuggerActions } from './debuggerActions';
import { VsCodeBreakpointUi } from './vscodeBreakpointUi';
import { CoreRef, splitCoreSuffix, withCoreSuffix } from './breakpointCondition';
import { SourceMappingStore, SourcePrefixMapping } from './sourceFileMap';
import { probeLostReason, targetHealthStatus } from './targetHealth';
import * as hub from './hub';

let extensionContext: vscode.ExtensionContext;
let agentOutput: vscode.OutputChannel | undefined;
let activeAurixSessions = 0;
let startingAurixSessions = 0;
let enforcingMemoryInspectorSettings = false;
const memoryInspectorExtensionId = 'eclipse-cdt.memory-inspector';
const openedDisassemblySessions = new Set<string>();
const aurixDebugSessions = new Set<vscode.DebugSession>();
let resolvedDisassemblyOpenCommand: string | undefined;
let instructionStepStatusItem: vscode.StatusBarItem | undefined;
const instructionStepStateKey = 'aurix-debugger.instructionSteppingEnabled';
let lastInstructionSteppingEnabled = false;
let liveWatchStatusItem: vscode.StatusBarItem | undefined;
let targetHealthItem: vscode.StatusBarItem | undefined;
/** Last `aurix/targetHealth` body per debug session ID, in the order they were reported. */
const targetHealthBySession = new Map<string, unknown>();
let liveWatches: LiveWatchManager | undefined;
let osViewProvider: OsViewProvider | undefined;
let debugLayout: DebugLayout | undefined;
const liveWatchIntervalStateKey = 'aurix-debugger.liveWatch.intervalMs';
let lastLiveWatchIntervalMs = 1000;
let preferredSourceViewColumn: vscode.ViewColumn | undefined;
let lastAutoSourcePath: string | undefined;
let lastAutoSourceLine: number | undefined;
/** Sources not found locally, already logged, per debug session ID. */
const missingSourcesLogged = new Map<string, Set<string>>();
/** Debug sessions in which "Locate File" was offered (once per session). */
const missingSourcePromptShown = new Set<string>();
const LEARNED_SOURCE_MAPPINGS_KEY = 'aurix-debugger.learnedSourceMappings';
let warnedSvdFileReplaced = false;
/** Command the XPeripherals view gets its peripherals from, instead of parsing the SVD file. */
const SVD_PERIPHERALS_COMMAND = 'aurix-debugger.svdPeripherals';
const DISASSEMBLY_OPEN_DELAY_MS = 1200;
const pendingDisassemblyOpenTimers = new Map<string, NodeJS.Timeout>();

export function activate(context: vscode.ExtensionContext) {
	// This method is called when the extension is activated
	extensionContext = context;
	console.log('AURIX debugger extension is activating...');
	agentOutput = vscode.window.createOutputChannel('AURIX Debug Server');
	liveWatches = new LiveWatchManager(context);
	debugLayout = new DebugLayout(context, liveWatches, (line) => agentOutput?.appendLine(line));
	context.subscriptions.push(liveWatches);
	context.subscriptions.push(
		// Called by the XPeripherals view at the session's first stop ("peripherals" in the launch configuration).
		vscode.commands.registerCommand(SVD_PERIPHERALS_COMMAND, (session?: vscode.DebugSession) => peripheralDefinitions(session)),
	);
	void removeOldSvdCache();

	void ensureMemoryInspectorSettings();
	applyLiveWatchPauseWhileHalted();
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
			if (event.affectsConfiguration('aurix-debugger.liveWatch.pauseWhileHalted')) {
				applyLiveWatchPauseWhileHalted();
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
			try {
				await s.customRequest('aurix/reset', {});
			} catch (e) {
				void vscode.window.showErrorMessage(`AURIX™ Debugger: ${e instanceof Error ? e.message : String(e)}`);
			}
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
				try {
					// The worker reports the progress in its log (followed into the AURIX Debug Server
					// output): its sector lines go into the notification.
					agentOutput?.show(true);
					await vscode.window.withProgress(
						{ location: vscode.ProgressLocation.Notification, title: 'AURIX™ Debugger: flashing target' },
						async (progress) => {
							hubLogListeners.set(s.id, (line) => {
								const m = /^(?:Info|Warn)\s*:\s*(.*(?:sectors|flash|erase|program|verif).*)$/i.exec(line);
								if (m) {
									progress.report({ message: m[1] });
								}
							});
							try {
								await s.customRequest('aurix/flash', {});
							} finally {
								hubLogListeners.delete(s.id);
							}
						}
					);
				} catch (e) {
					void vscode.window.showErrorMessage(`AURIX™ Debugger: flashing failed: ${e instanceof Error ? e.message : String(e)}`);
					return;
				}
				if (s.configuration.ucbWrite === true) {
					// Flashing skipped the UCB records: review them; the UCBs panel writes them on confirmation.
					await UcbPanel.reviewFlashImages(context.extensionUri, s);
				}
				try {
					// Same as the Reset button: the launch configuration decides whether the target halts or runs.
					await s.customRequest('aurix/reset', {});
					void vscode.window.showInformationMessage('AURIX™ Debugger: flashing done.');
				} catch (e) {
					void vscode.window.showErrorMessage(`AURIX™ Debugger: flashed, but ${e instanceof Error ? e.message : String(e)}`);
				}
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
			const picked = await pickIntervalMs({
				title: 'Global Live Update Interval',
				placeHolder: 'How often the live views re-read the target while it runs',
				current: await getLiveWatchIntervalMs(),
			});
			if (typeof picked === 'number') {
				await setLiveWatchInterval(picked);
			}
		})
	);

	// Live Watch panels. The commands without an argument act on the sidebar; the panel's title bar
	// has its own ones, which act on the active panel.
	const activePanel = () => liveWatches?.activePanelInstance();
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchOpenPanel', async () => {
			await liveWatches?.showPanelPicker();
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchConfigureInterval', async (item?: LiveWatchCommandArgument) => {
			if (liveWatches) {
				await liveWatches.configureInterval(liveWatches.resolve(item));
				updateLiveWatchStatus(activeAurixSessions > 0);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelAddExpression', async () => {
			const instance = activePanel();
			if (instance) {
				await instance.addExpression(await pickLiveWatchExpression());
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelRefresh', async () => {
			await activePanel()?.refreshNow();
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelConfigureInterval', async () => {
			const instance = activePanel();
			if (instance && liveWatches) {
				await liveWatches.configureInterval(instance);
				updateLiveWatchStatus(activeAurixSessions > 0);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelClearExpressions', async () => {
			await activePanel()?.clearExpressions();
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelRename', async () => {
			await liveWatches?.renamePanel(activePanel()?.id);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchPanelMoveToNewWindow', async () => {
			await liveWatches?.moveActivePanelToNewWindow();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchAddExpression', async () => {
			await liveWatches?.sidebar.addExpression(await pickLiveWatchExpression());
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchRemoveExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).removeExpression(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchSetValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).setValue(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchPauseExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).setExpressionPaused(item, true);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchResumeExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).setExpressionPaused(item, false);
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
			const target = await liveWatches?.pickTarget(`Add ${entry.label} to Live Watch`);
			if (target && liveWatches) {
				await target.addPeripheral(entry);
				await liveWatches.reveal(target);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.editorBreakOnChange', () => editorDataBreakpoint('write')),
		vscode.commands.registerCommand('aurix-debugger.editorBreakOnRead', () => editorDataBreakpoint('read')),
		vscode.commands.registerCommand('aurix-debugger.editorBreakOnAccess', () => editorDataBreakpoint('readWrite')),
		vscode.commands.registerCommand('aurix-debugger.liveWatchBreakOnChange', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).addDataBreakpoint(item, 'write');
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchBreakOnRead', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).addDataBreakpoint(item, 'read');
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchBreakOnAccess', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).addDataBreakpoint(item, 'readWrite');
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchEditExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).editExpression(item);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchCopyValue', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).copyValue(item);
		}),
		vscode.commands.registerCommand('aurix-debugger.liveWatchCopyExpression', async (item?: LiveWatchCommandArgument) => {
			await liveWatches?.resolve(item).copyExpression(item);
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchClearExpressions', async () => {
			await liveWatches?.sidebar.clearExpressions();
		})
	);

	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.liveWatchRefresh', async () => {
			await liveWatches?.sidebar.refreshNow();
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
	registerTraceStatus(context);
	registerReverseUi(context);
	liveWatchStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
	liveWatchStatusItem.command = 'aurix-debugger.configureLiveWatch';
	liveWatchStatusItem.tooltip = 'Configure the global live update interval';
	context.subscriptions.push(liveWatchStatusItem);
	// Probe/target health from the debug server (AurixTarget service); hidden while all is well.
	targetHealthItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 98);
	targetHealthItem.name = 'AURIX Target Health';
	context.subscriptions.push(targetHealthItem);
	// Like the Watch view, every Live Watch updates on a break, also for paused expressions and when
	// the periodic refresh is off. The adapter's stopped event also covers cores stopping without focus.
	context.subscriptions.push(
		vscode.debug.registerDebugAdapterTrackerFactory('aurixdbg', {
			createDebugAdapterTracker: (session) => ({
				// A debug session whose launch failed never reaches onDidTerminateDebugSession: its hub
				// session (and the probe it holds) would stay open. hubSessionEnded runs once per session.
				onWillStopSession: () => void hubSessionEnded(session),
				onExit: () => void hubSessionEnded(session),
				onDidSendMessage: (message: { type?: string; event?: string; command?: string; success?: boolean; body?: { allHalted?: unknown } }) => {
					if (message?.type === 'response' && message.command === 'launch' && message.success === false) {
						void hubSessionEnded(session);
					}
					if (message?.type === 'event' && message.event === 'stopped') {
						// Paused live views still update on a halt, the way the Watch view does.
						notifyLiveUpdateBreak();
						// Reverse debugging: the history grew or was cut.
						reverseStopped();
					}
					if (message?.type === 'event' && message.event === 'aurix/runState') {
						// Every TriCore core halted: the periodic loops rest (liveWatch.pauseWhileHalted).
						setLiveUpdateTargetHalted(message.body?.allHalted === true);
					}
				},
			}),
		})
	);

	osViewProvider = new OsViewProvider();
	context.subscriptions.push(osViewProvider);
	// A TreeView, not just a provider: only a view can say whether anyone is looking, and a
	// hidden AUTOSAR OS section must read nothing.
	const osTree = vscode.window.createTreeView(OsViewProvider.viewId, { treeDataProvider: osViewProvider });
	osViewProvider.attach(osTree);
	context.subscriptions.push(osTree);
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.osRefresh', () => osViewProvider?.refresh()),
		vscode.commands.registerCommand('aurix-debugger.osTogglePause', () => osViewProvider?.togglePaused()),
		vscode.commands.registerCommand('aurix-debugger.resetDebugLayout', async () => {
			await debugLayout?.reset();
			void vscode.window.showInformationMessage('Debug layout forgotten: the next session opens the default layout.');
		}),
		vscode.commands.registerCommand('aurix-debugger.osShowProfile', () => OsProfilePanel.show(context.extensionUri)),
		vscode.commands.registerCommand('aurix-debugger.showTrace', () => TracePanel.show(context)),
		vscode.commands.registerCommand('aurix-debugger.traceReplayExit', async () => {
			const s = vscode.debug.activeDebugSession;
			if (s?.type === 'aurixdbg') {
				await s.customRequest('aurix/traceReplay', { off: true });
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.showUcbs', () => UcbPanel.show(context.extensionUri)),
		vscode.commands.registerCommand('aurix-debugger.osShowObjects', (kind?: string) =>
			OsObjectsPanel.show(context.extensionUri, typeof kind === 'string' ? kind : undefined),
		),
		vscode.commands.registerCommand('aurix-debugger.osBreakInTask', async (where?: LineNumberMenuArgument) => {
			const found = breakpointsForCommand(where);
			if (!found) {
				return;
			}
			const { session, here } = found;

			let tasks: string[] = [];
			try {
				const list = (await session.customRequest('aurix/osObjects', { kind: 'TASK', liveOnly: true })) as { Objects?: Array<{ Name: string }> };
				tasks = (list.Objects ?? []).map(o => o.Name);
			} catch (e) {
				void vscode.window.showErrorMessage(`Cannot list tasks: ${describeOsError(e)}`);
				return;
			}
			if (tasks.length === 0) {
				void vscode.window.showWarningMessage('No ORTI file is loaded, so there are no tasks to pick from.');
				return;
			}

			const CLEAR = 'Any task (remove the filter)';
			const picked = await vscode.window.showQuickPick([CLEAR, ...tasks], {
				title: 'Halt only in this task',
				placeHolder: 'The breakpoint still halts the core every time; the debugger resumes it for other tasks',
			});
			if (picked === undefined) {
				return;
			}
			// The cores (`@N`) stay; only the task part is replaced.
			replaceConditions(here, b => withCoreSuffix(picked === CLEAR ? '' : `task == ${picked}`, splitCoreSuffix(b.condition ?? '').cores));
		}),
		vscode.commands.registerCommand('aurix-debugger.breakOnCore', async (where?: LineNumberMenuArgument) => {
			const found = breakpointsForCommand(where);
			if (!found) {
				return;
			}
			const { session, here } = found;

			let cores: Array<{ core: number; id: string; name: string; suspended: boolean; hsm?: boolean; scr?: boolean }> = [];
			try {
				cores = ((await session.customRequest('aurix/cores')) as { cores?: typeof cores }).cores ?? [];
			} catch (e) {
				void vscode.window.showErrorMessage(`Cannot list the cores: ${e instanceof Error ? e.message : String(e)}`);
				return;
			}
			if (cores.length === 0) {
				void vscode.window.showWarningMessage('The target has no cores to pick from.');
				return;
			}

			const current = new Set<CoreRef>(splitCoreSuffix(here[0].condition ?? '').cores ?? []);
			const picked = await vscode.window.showQuickPick(
				// Named as in the Call Stack; the description shows what the condition will say.
				cores.map(c => {
					// The HSM is written `@hsm`, the SCR `@scr`, though a typed `@6` (the number) means it too.
					const core: CoreRef = c.hsm ? 'hsm' : c.scr ? 'scr' : c.core;
					return {
						label: c.name,
						description: `@${core} · ${c.suspended ? 'suspended' : 'running'}`,
						picked: current.has(core) || current.has(c.core),
						core,
					};
				}),
				{
					title: 'Halt only on these cores',
					placeHolder: 'Other cores never halt here. Pick none to halt on every core.',
					canPickMany: true,
				},
			);
			if (picked === undefined) {
				return;
			}
			// The task filter stays; only the `@N` part is replaced.
			replaceConditions(here, b => withCoreSuffix(b.condition ?? '', picked.map(p => p.core)));
		}),
		vscode.commands.registerCommand('aurix-debugger.osLoadFile', async () => {
			const picked = await vscode.window.showOpenDialog({
				canSelectMany: false,
				openLabel: 'Load ORTI file',
				filters: { 'ORTI files': ['ORT', 'ort', 'orti', 'ORTI'], 'All files': ['*'] },
			});
			const file = picked?.[0]?.fsPath;
			if (!file) {
				return;
			}
			try {
				await osViewProvider?.load(file);
			} catch (e) {
				void vscode.window.showErrorMessage(`Cannot load ORTI file: ${describeOsError(e)}`);
			}
		}),
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
				void ensureMemoryInspectorSettings();
				void syncInstructionSteppingOnSessionStart(session);
				void syncLiveWatchOnSessionStart(session);
				void liveWatches?.refreshAll();
				if (activeAurixSessions === 1) {
					void debugLayout?.sessionStarted();
				}
			}
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidChangeActiveDebugSession(() => {
			void liveWatches?.refreshAll();
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
			// While the Disassembly view is the active editor, VS Code shows every focused frame there
			// and not in its source: reveal the source next to it, keeping the focus in the view.
			const activeTab = vscode.window.tabGroups.activeTabGroup.activeTab;
			if (activeTab && activeTab.input === undefined && activeTab.label === 'Disassembly') {
				const frameId = (item as vscode.DebugStackFrame).frameId;
				void Promise.resolve(session.customRequest('aurix/frameSource', { frameId }))
					.then((src: { path?: string; line?: number }) => {
						if (src?.path && src.line) {
							void autoRevealSourcePath(src.path, src.line, session, true);
						}
					}, () => undefined);
			}
		})
	);
	context.subscriptions.push(
		vscode.debug.onDidReceiveDebugSessionCustomEvent((e) => {
			if (e.session.type !== 'aurixdbg') {
				return;
			}
			if (e.event === 'aurix/ucbImages') {
				// The launch flashed images with UCB records: review them, write only on confirmation.
				void UcbPanel.reviewFlashImages(context.extensionUri, e.session);
				return;
			}
			if (e.event === 'aurix/targetHealth') {
				// Re-inserted so the most recently reporting session is shown.
				targetHealthBySession.delete(e.session.id);
				targetHealthBySession.set(e.session.id, e.body);
				updateTargetHealthItem();
				return;
			}
			if (e.event === 'aurix/traceReplay') {
				// The debugger shows a trace position (Step Back) or the live core again.
				const b = e.body as { active?: boolean; at?: number } | undefined;
				void vscode.commands.executeCommand('setContext', 'aurix.traceReplay', Boolean(b?.active));
				TracePanel.replayMoved(Boolean(b?.active), typeof b?.at === 'number' ? b.at : undefined);
				traceReplay(b);
				return;
			}
			if (e.event === 'aurix/traceChanged') {
				// The launch configuration armed the trace or set arming on every run.
				TracePanel.traceChanged();
				return;
			}
			if (e.event === 'aurix/reverse') {
				// Reverse debugging switched on or off, or its history changed.
				reverseStatus(e.body);
				return;
			}
			if (e.event === 'aurix/probeLost') {
				void vscode.window.showErrorMessage('Debug probe lost: ' + probeLostReason(e.body));
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
			void autoRevealSourcePath(srcPath, line, e.session);
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
			missingSourcesLogged.delete(session.id);
			missingSourcePromptShown.delete(session.id);
			aurixDebugSessions.delete(session);
			targetHealthBySession.delete(session.id);
			activeAurixSessions =Math.max(0, activeAurixSessions - 1);
			if (activeAurixSessions === 0) {
				targetHealthBySession.clear();
			}
			updateTargetHealthItem();
			if (activeAurixSessions === 0) {
				updateInstructionSteppingStatus(lastInstructionSteppingEnabled, false);
				updateLiveWatchStatus(false);
				setLiveUpdateSessionActive(false);
				void debugLayout?.sessionsEnded();
			}
			// The hub session: closed if this debug session opened it (sessionLifetime "close").
			void hubSessionEnded(session);
		})
	);

	context.subscriptions.push({
		dispose: () => {
			stopHubEvents?.();
			for (const b of hubBindings.values()) {
				b.relay?.close();
			}
		}
	});
	registerHubCommands(context);
	void installHubOnActivation().finally(() => followHubEvents());

	void vscode.window.showInformationMessage('AURIX debugger extension is enabled.');
}

export class AurixTCFDebugAdapterFactory implements vscode.DebugAdapterDescriptorFactory {
    // Create the debug adapter descriptor for a debug session
	async createDebugAdapterDescriptor(session: vscode.DebugSession): Promise<vscode.ProviderResult<vscode.DebugAdapterDescriptor>> {
		startingAurixSessions++;
		try {
		const config = session.configuration;
		let endpoint: HubEndpoint;
		try {
			endpoint = await prepareHubSession(session, config);
		} catch (e) {
			const message = e instanceof Error ? e.message : String(e);
			agentOutput?.appendLine(message);
			agentOutput?.show(true);
			void vscode.window.showErrorMessage(`AURIX: ${message.split('\n')[0]}`);
			throw e;
		}

		// Enable path mapper based on workspace trust and configuration
		const pathMapperConfigDefault = vscode.workspace.getConfiguration('aurix-debugger').inspect("executePathMapper")?.defaultValue ?? true;
		const pathMapperConfig = vscode.workspace.getConfiguration('aurix-debugger').get('executePathMapper', pathMapperConfigDefault);

		const enablePathMapper = vscode.workspace.isTrusted && pathMapperConfig === true;
		
		const adapter = new AurixTCFDebugSession(enablePathMapper, getDisassemblyOpcodeBytes(), new VsCodeBreakpointUi(), sourceMappingStore());
		// Changes to session.configuration do not reach the launch request: the adapter gets the
		// hub session's TCF endpoint directly.
		adapter.connectTo(endpoint);
		return new vscode.DebugAdapterInlineImplementation(adapter);
		} finally {
			startingAurixSessions = Math.max(0, startingAurixSessions - 1);
		}
	}
}

/** What VS Code passes to a command run from the editor's line number context menu. */
type LineNumberMenuArgument = { lineNumber?: number; uri?: vscode.Uri } | undefined;

/**
 * The AURIX session and the source breakpoints a breakpoint command acts on: those on the line
 * whose number was right-clicked, else on the cursor's line, which is what "this breakpoint"
 * means. Tells the user and returns undefined when there are none.
 */
function breakpointsForCommand(where: LineNumberMenuArgument): { session: vscode.DebugSession; here: vscode.SourceBreakpoint[] } | undefined {
	const session = vscode.debug.activeDebugSession;
	if (session?.type !== 'aurixdbg') {
		void vscode.window.showWarningMessage('Start an AURIX debug session first.');
		return undefined;
	}
	const editor = vscode.window.activeTextEditor;
	const uri = where?.uri ?? editor?.document.uri;
	// `lineNumber` is 1-based.
	const line = typeof where?.lineNumber === 'number' ? where.lineNumber - 1 : editor?.selection.active.line;
	if (!uri || line === undefined) {
		void vscode.window.showWarningMessage('Open the file with the breakpoint first.');
		return undefined;
	}
	const here = vscode.debug.breakpoints.filter(
		(b): b is vscode.SourceBreakpoint =>
			b instanceof vscode.SourceBreakpoint &&
			b.location.uri.toString() === uri.toString() &&
			b.location.range.start.line === line,
	);
	if (here.length === 0) {
		void vscode.window.showWarningMessage('No breakpoint on this line.');
		return undefined;
	}
	return { session, here };
}

/**
 * A data breakpoint on the editor's selection, or on the identifier under the cursor. Evaluated in
 * the focused stack frame when a core is halted, so a local is watched where it lives; otherwise
 * as the Live Watch evaluates it.
 */
async function editorDataBreakpoint(access: 'write' | 'read' | 'readWrite'): Promise<void> {
	const session = vscode.debug.activeDebugSession;
	if (session?.type !== 'aurixdbg') {
		void vscode.window.showWarningMessage('Start an AURIX debug session first.');
		return;
	}
	const editor = vscode.window.activeTextEditor;
	if (!editor) {
		return;
	}
	const doc = editor.document;
	const sel = editor.selection;
	const word = sel.isEmpty ? doc.getWordRangeAtPosition(sel.active, /[A-Za-z_]\w*/) : undefined;
	const expression = (sel.isEmpty ? (word ? doc.getText(word) : '') : doc.getText(sel)).trim();
	if (expression === '' || expression.includes('\n')) {
		void vscode.window.showWarningMessage('Select an expression, or put the cursor on a variable.');
		return;
	}
	// The frame selected in the Call Stack (VS Code 1.90 and later).
	const item = (vscode.debug as { activeStackItem?: { session?: vscode.DebugSession; frameId?: number } }).activeStackItem;
	const frameId = item?.session === session && typeof item.frameId === 'number' ? item.frameId : undefined;
	try {
		const r = await session.customRequest('aurix/addDataBreakpoint', { expression, access, inFrame: true, frameId }) as { name?: string } | undefined;
		vscode.window.setStatusBarMessage(`Data breakpoint ${r?.name ?? expression}`, 4000);
	} catch (e) {
		void vscode.window.showErrorMessage(e instanceof Error ? e.message : String(e));
	}
}

/** Gives breakpoints new conditions; an empty one removes the condition. */
function replaceConditions(bps: vscode.SourceBreakpoint[], condition: (b: vscode.SourceBreakpoint) => string): void {
	// A breakpoint's condition cannot be edited in place, so it is replaced.
	const replaced = bps.map(b => new vscode.SourceBreakpoint(b.location, b.enabled, condition(b) || undefined, b.hitCondition, b.logMessage));
	vscode.debug.removeBreakpoints(bps);
	vscode.debug.addBreakpoints(replaced);
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



/** Where the extension unpacked and converted SVD files before the debug server took that over. */
function oldSvdCacheDirs(): string[] {
	const dirs = [path.join(extensionContext.extensionPath, '.cache', 'svd-cache')];
	const storage = extensionContext?.globalStorageUri?.fsPath;
	if (typeof storage === 'string' && storage.trim() !== '') {
		dirs.push(path.join(storage, 'svd-cache'));
	}
	return dirs;
}

/** Removes the SVD cache of earlier versions (unpacked SVD files and converted definitions, 100+ MB). */
async function removeOldSvdCache(): Promise<void> {
	for (const dir of oldSvdCacheDirs()) {
		if (fs.existsSync(dir)) {
			await fs.promises.rm(dir, { recursive: true, force: true }).catch(() => undefined);
		}
	}
}

/**
 * The XPeripherals view's definitions for a debug session: its debug server converts the bundled
 * SVD file of the launch configuration's "device" (with the files merged into it) and loads it
 * for the session's peripheral operations too.
 */
async function peripheralDefinitions(session?: vscode.DebugSession): Promise<PeripheralOptionsList | undefined> {
	const binding = session ? hubBindings.get(session.id) : undefined;
	const device = session?.configuration?.device;
	if (!binding || typeof device !== 'string' || device.trim() === '') {
		return undefined;
	}
	const t0 = Date.now();
	try {
		const list = await binding.client.request('GET', `/api/v1/sessions/${encodeURIComponent(binding.session)}/svd/definitions?device=${encodeURIComponent(device.trim())}`, undefined, 120_000);
		if (!Array.isArray(list)) {
			return undefined;
		}
		agentOutput?.appendLine(`XPeripherals: ${list.length} peripherals of ${device.trim()} from the debug server in ${((Date.now() - t0) / 1000).toFixed(1)}s`);
		return PeripheralOptionsList.from(list) as PeripheralOptionsList;
	} catch (e) {
		agentOutput?.appendLine(`XPeripherals: no peripherals for ${device.trim()}: ${e instanceof Error ? e.message : String(e)}`);
		return undefined;
	}
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
				// The view takes its definitions from the debug server, which converts the bundled
				// SVD files (parsing one in the extension host would block it for seconds). The key
				// of the view's cache of built peripherals names the device and the build.
				if ((config as any).peripherals === undefined) {
					(config as any).peripherals = `command:${SVD_PERIPHERALS_COMMAND}`;
					(config as any).peripheralsCacheKey = `aurix-svd:${entry.id}:${extensionContext.extension.packageJSON.version}`;
				}
			} else {
				void vscode.window.showWarningMessage(`AURIX™ Debugger: unknown device "${device}"; the XPeripherals view is unavailable.`);
			}
		}

		return config;
	}
}

// --- aurix-hub: sessions, relay, commands --------------------------------------------------------

/** What the extension did with the hub for one debug session. */
interface HubBinding {
	client: hub.HubClient;
	session: string;
	name: string;
	/** The extension opened the session (else it attached to one). */
	opened: boolean;
	lifetime: 'close' | 'keep';
	relay?: hub.TcfRelay;
	/** Stops following the session's log into the output channel. */
	stopLog?: () => void;
}

const hubBindings = new Map<string, HubBinding>();

/** Per debug session: gets each new line of its hub session's log (the Flash Target progress). */
const hubLogListeners = new Map<string, (line: string) => void>();

/**
 * Follows the hub session's log into the output channel: the worker writes its progress
 * (connecting, flashing sector by sector) only there. A session this debug session opened is shown
 * from its start, an attached one from its last lines.
 */
async function followHubLog(sessionId: string, b: HubBinding): Promise<void> {
	let offset = 0;
	if (!b.opened) {
		try {
			const r = await b.client.sessionLog(b.session, 20);
			agentOutput?.appendLine(r.text);
			offset = r.next;
		} catch {
			// Followed from the start below.
		}
	}
	b.stopLog = b.client.followLog(b.session, offset, (text) => {
		agentOutput?.append(text);
		const listener = hubLogListeners.get(sessionId);
		if (listener) {
			for (const line of text.split(/\r?\n/)) {
				if (line.trim() !== '') {
					listener(line);
				}
			}
		}
	});
}
let stopHubEvents: (() => void) | undefined;

function hubSetting<T>(key: string, fallback: T): T {
	return vscode.workspace.getConfiguration('aurix-debugger').get<T>(key, fallback);
}

/** The hub URL and token of a launch configuration (or the settings). */
function hubTarget(config?: vscode.DebugConfiguration): { url: string; token?: string } {
	const url = String(config?.hub || hubSetting<string>('hub.url', hub.DEFAULT_HUB_URL) || hub.DEFAULT_HUB_URL).replace(/\/+$/, '');
	const token = (config?.hubToken as string | undefined) || hubSetting<string>('hub.token', '') || process.env.AURIX_HUB_TOKEN || undefined;
	return { url, token };
}

function asStringList(v: unknown): string[] {
	if (Array.isArray(v)) {
		return v.filter((x): x is string => typeof x === 'string' && x.trim() !== '');
	}
	return typeof v === 'string' && v.trim() !== '' ? [v] : [];
}

/** A local file for a hub on another machine: uploaded, replaced by its `file:` ID. */
async function hubPath(client: hub.HubClient, p: string): Promise<string> {
	if (client.local || !fs.existsSync(p)) {
		return p;
	}
	return client.upload(p);
}

function describeLaunch(l: hub.LaunchStatus | undefined): string {
	if (!l) {
		return '';
	}
	return l.steps.map((s) => `${s.ok ? '✓' : '✗'} ${s.step} (${s.ms} ms)${s.detail ? `: ${s.detail.split('\n').slice(-6).join('\n    ')}` : ''}`).join('\n');
}

/** Where the debug adapter reaches the TCF server of its hub session. */
type HubEndpoint = { host: string; port: number; hubFlashed?: boolean };

/**
 * Opens (or attaches to) the hub session of a debug session and returns where the debug adapter
 * reaches its TCF server: directly on this machine, through a local relay to a hub on another one.
 */
async function prepareHubSession(session: vscode.DebugSession, config: vscode.DebugConfiguration): Promise<HubEndpoint> {
	const { url, token } = hubTarget(config);
	const client = new hub.HubClient({ url, token, clientId: `vscode:${session.id}` });
	const log = (l: string) => agentOutput?.appendLine(l);
	if (client.local) {
		await hub.ensureLocalHub(client, extensionContext.extensionPath, log);
	} else if (!(await client.health())) {
		throw new Error(`aurix-hub at ${url} does not answer`);
	}

	let info: hub.SessionInfo | undefined;
	let opened = false;
	if (typeof config.session === 'string' && config.session.trim() !== '') {
		info = await client.findSession(config.session.trim());
		if (!info || info.state === 'exited') {
			throw new Error(`No open session "${config.session}" on ${url} (see AURIX: Show Debugger Hub Status)`);
		}
		log(`Attaching to session ${info.name} (${info.session})`);
		if (info.state === 'starting') {
			const st = await client.waitSession(info.session, 600);
			if (st.phase === 'failed') {
				throw new Error(`Session ${info.name} failed to start: ${st.error ?? ''}`);
			}
			info = (await client.findSession(info.session)) ?? info;
		}
	} else {
		const configFiles: string[] = [];
		for (const f of asStringList(config.configFiles)) {
			// Names like "board/infineon/x.cfg" are looked up in the search directories; only a
			// file that exists relative to the workspace is passed as a path.
			const resolved = resolveConfigPath(session, f);
			configFiles.push(resolved && fs.existsSync(resolved) ? await hubPath(client, resolved) : f);
		}
		const searchDirs = asStringList(config.searchDirs).map((d) => resolveConfigPath(session, d) ?? d);
		const launch: Record<string, unknown> = {};
		const flashFiles = (Array.isArray(config.flashFiles) ? config.flashFiles : [])
			.map((f: any) => (typeof f === 'string' ? { path: f } : f))
			.filter((f: any) => f && typeof f.path === 'string' && f.path.trim() !== '');
		if (config.enableFlashing === true) {
			if (flashFiles.length === 0) {
				throw new Error('"enableFlashing" is set, but the launch configuration has no "flashFiles"');
			}
			const files = [];
			for (const f of flashFiles) {
				const p = await hubPath(client, resolveConfigPath(session, f.path) ?? f.path);
				files.push(f.offset !== undefined || f.type ? { path: p, offset: f.offset, type: f.type } : p);
			}
			launch.flash = { files, verify: config.verifyFlash !== false };
			launch.reset = { halt: config.resetSuspend === true, type: typeof config.resetType === 'string' ? config.resetType : undefined };
		}
		if (typeof config.runTo === 'string' && config.runTo.trim() !== '') {
			launch.run_to = config.runTo.trim();
		}
		const req: Record<string, unknown> = {
			name: typeof config.sessionName === 'string' && config.sessionName ? config.sessionName : config.name,
			wait_s: 3600,
		};
		if (typeof config.profile === 'string' && config.profile.trim() !== '') {
			req.profile = config.profile.trim();
		}
		if (configFiles.length || asStringList(config.configCommands).length || searchDirs.length || config.adapter || config.probe || config.ucbWrite === true) {
			req.connect = {
				config_files: configFiles,
				commands: asStringList(config.configCommands),
				search_dirs: searchDirs,
				adapter: typeof config.adapter === 'string' && config.adapter ? config.adapter : undefined,
				probe: typeof config.probe === 'string' && config.probe ? config.probe : undefined,
				// Writing UCBs (after the confirmation in the UCBs panel) only when the launch configuration asks.
				ucb_write: config.ucbWrite === true ? true : undefined,
			};
		}
		if (!req.profile && !req.connect) {
			throw new Error('The launch configuration names no board: set "profile" (a hub profile), "configFiles", or "session" to attach to an open session');
		}
		if (Object.keys(launch).length) {
			req.launch = launch;
		}
		// The hub of this machine closes the session when this extension host ends (VS Code closed,
		// crashed or killed), also while the launch still runs.
		if (client.local && config.sessionLifetime !== 'keep') {
			req.owner_pid = process.pid;
		}
		agentOutput?.show(true);
		log(`Opening a session on ${url}: ${JSON.stringify(req)}`);
		try {
			info = await vscode.window.withProgress(
				{ location: vscode.ProgressLocation.Notification, title: 'AURIX: opening the debug session…' },
				async () => {
					try {
						return await client.openSession(req);
					} catch (e) {
						// A hub older than the extension (its update waits for its sessions to close).
						if (e instanceof hub.HubError && e.status === 400 && req.owner_pid !== undefined && /owner_pid/.test(e.message)) {
							log('The hub does not know owner_pid yet: the session is closed with VS Code only when VS Code ends normally');
							delete req.owner_pid;
							return client.openSession(req);
						}
						throw e;
					}
				},
			);
		} catch (e) {
			const body = e instanceof hub.HubError ? e.body : undefined;
			throw new Error(`Opening the session failed: ${e instanceof Error ? e.message : String(e)}${body?.status ? ` (${body.status})` : ''}`);
		}
		opened = true;
		log(`Session ${info.name} (${info.session}) is ${info.state}, launch ${info.launch.phase}\n${describeLaunch(info.launch)}`);
		if (info.launch.phase === 'starting') {
			info.launch = await client.waitSession(info.session, 3600);
		}
		if (info.launch.phase === 'failed') {
			const lifetimeOnError = config.sessionLifetime === 'keep' ? 'kept open' : 'closed';
			if (config.sessionLifetime !== 'keep') {
				void client.closeSession(info.session).catch(() => undefined);
			}
			throw new Error(`The launch failed (${lifetimeOnError}): ${info.launch.error ?? ''}\n${describeLaunch(info.launch)}`);
		}
	}
	if (!info?.tcf_port) {
		throw new Error(`Session ${info?.name ?? ''} has no TCF port`);
	}
	const binding: HubBinding = {
		client,
		session: info.session,
		name: info.name,
		opened,
		lifetime: config.sessionLifetime === 'keep' ? 'keep' : 'close',
	};
	let port = info.tcf_port;
	if (!client.local) {
		binding.relay = await hub.startTcfRelay(client, info.session, log);
		port = binding.relay.port;
	}
	hubBindings.set(session.id, binding);
	await followHubLog(session.id, binding);
	// The hub flashed and reset when it opened the session: the debug adapter must not flash again.
	return { host: '127.0.0.1', port, hubFlashed: opened && config.enableFlashing === true };
}

/** Hub sessions being closed or left (`hubSessionEnded`): `deactivate` waits for them. */
const pendingHubEnds = new Set<Promise<void>>();

/** A debug session ended: close the relay, and the hub session if the extension opened it. */
function hubSessionEnded(session: vscode.DebugSession): Promise<void> {
	return endHubBinding(session.id);
}

function endHubBinding(id: string): Promise<void> {
	const b = hubBindings.get(id);
	if (!b) {
		return Promise.resolve();
	}
	hubBindings.delete(id);
	hubLogListeners.delete(id);
	const p = finishHubBinding(b).finally(() => pendingHubEnds.delete(p));
	pendingHubEnds.add(p);
	return p;
}

async function finishHubBinding(b: HubBinding): Promise<void> {
	b.stopLog?.();
	b.relay?.close();
	try {
		if (b.opened && b.lifetime === 'close') {
			agentOutput?.appendLine(`Closing session ${b.name} (${b.session})`);
			await b.client.closeSession(b.session);
		} else {
			await b.client.endClient(b.session);
		}
	} catch (e) {
		agentOutput?.appendLine(`Ending session ${b.name}: ${e instanceof Error ? e.message : String(e)}`);
	}
}

/** Copies the bundled hub to the fixed per-user location when it is newer (on activation). */
async function installHubOnActivation(): Promise<void> {
	if (process.platform !== 'win32' || hubSetting<string>('hub.install', 'auto') !== 'auto') {
		return;
	}
	const exe = hub.bundledHubExe(extensionContext.extensionPath);
	if (!fs.existsSync(exe)) {
		return;
	}
	const r = await hub.runHub(exe, ['install', '--extension', extensionContext.extensionPath]);
	if (r.out) {
		agentOutput?.appendLine(`aurix-hub install: ${r.out}`);
	}
	if (r.code !== 0) {
		agentOutput?.appendLine(`aurix-hub install failed (code ${r.code})`);
	}
}

/** Offers to attach VS Code to sessions that other clients (Claude, scripts) open. */
function followHubEvents(): void {
	stopHubEvents?.();
	stopHubEvents = undefined;
	if (!hubSetting<boolean>('hub.offerAttach', true)) {
		return;
	}
	const { url, token } = hubTarget();
	const client = new hub.HubClient({ url, token, clientId: 'vscode:events' });
	stopHubEvents = client.followEvents((ev) => {
		if (ev?.type !== 'session_launched' || ev.phase !== 'ready') {
			return;
		}
		void (async () => {
			const info = await client.findSession(ev.session).catch(() => undefined);
			if (!info || String(info.opened_by ?? '').startsWith('vscode:')) {
				return;
			}
			if ([...hubBindings.values()].some((b) => b.session === info.session)) {
				return;
			}
			const pick = await vscode.window.showInformationMessage(
				`AURIX: session "${info.name}" (${info.targets.join(', ')}) was opened by ${info.opened_by ?? 'another client'}. Attach the debugger?`,
				'Attach',
			);
			if (pick === 'Attach') {
				await attachToHubSession(info);
			}
		})();
	});
}

async function attachToHubSession(info: hub.SessionInfo): Promise<void> {
	const folder = vscode.workspace.workspaceFolders?.[0];
	await vscode.debug.startDebugging(folder, {
		type: 'aurixdbg',
		request: 'launch',
		name: `AURIX: ${info.name}`,
		session: info.session,
		sessionLifetime: 'keep',
	});
}

function registerHubCommands(context: vscode.ExtensionContext): void {
	const bundled = () => hub.bundledHubExe(context.extensionPath);
	const report = (title: string, r: { code: number; out: string }) => {
		agentOutput?.appendLine(`${title}:\n${r.out}`);
		if (r.code === 0) {
			void vscode.window.showInformationMessage(`${title}: ${r.out.split('\n').filter((l) => l.trim()).slice(-1)[0] ?? 'done'}`);
		} else {
			agentOutput?.show(true);
			void vscode.window.showErrorMessage(`${title} failed: ${r.out}`);
		}
	};
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-debugger.hubInstallAutostart', async () => {
			report('Start debugger hub at logon', await hub.runHub(bundled(), ['install', '--autostart', '--extension', context.extensionPath]));
		}),
		vscode.commands.registerCommand('aurix-debugger.hubRemoveAutostart', async () => {
			report("Don't start debugger hub at logon", await hub.runHub(hub.hubExe(context.extensionPath), ['uninstall', '--autostart-only']));
		}),
		vscode.commands.registerCommand('aurix-debugger.hubStatus', async () => {
			const { url, token } = hubTarget();
			const client = new hub.HubClient({ url, token, clientId: 'vscode:status' });
			const lines: string[] = [`Hub: ${url}`];
			const h = await client.health();
			if (!h) {
				lines.push('  not running');
			} else {
				const info = await client.request('GET', '/api/v1/hub', undefined, 10_000).catch(() => undefined);
				lines.push(`  aurix-hub ${h.version}, pid ${h.pid}, ${info?.autostart ? 'starts at logon' : 'not started at logon'}${info?.update_pending ? ', update pending' : ''}`);
				lines.push(`  log: ${info?.log_file ?? ''}`);
				for (const s of await client.listSessions().catch(() => [])) {
					lines.push(`  session ${s.name} (${s.session}): ${s.state}, launch ${s.launch.phase}, targets ${s.targets.join(', ')}${s.control ? `, control: ${s.control}` : ''}${s.opened_by ? `, opened by ${s.opened_by}` : ''}`);
				}
			}
			if (process.platform === 'win32') {
				const st = await hub.runHub(hub.hubExe(context.extensionPath), ['status'], 20_000);
				lines.push(`Installation:\n${st.out}`);
			}
			agentOutput?.appendLine(lines.join('\n'));
			agentOutput?.show(true);
		}),
		vscode.commands.registerCommand('aurix-debugger.hubRestart', async () => {
			const ok = await vscode.window.showWarningMessage('Restart the debugger hub now? Its open sessions are closed.', { modal: true }, 'Restart');
			if (ok !== 'Restart') {
				return;
			}
			const exe = hub.hubExe(context.extensionPath);
			const stop = await hub.runHub(exe, ['stop', '--now']);
			agentOutput?.appendLine(stop.out);
			const { url, token } = hubTarget();
			try {
				await hub.ensureLocalHub(new hub.HubClient({ url, token, clientId: 'vscode:restart' }), context.extensionPath, (l) => agentOutput?.appendLine(l));
				void vscode.window.showInformationMessage('The debugger hub was restarted.');
			} catch (e) {
				void vscode.window.showErrorMessage(`Restarting the debugger hub failed: ${e instanceof Error ? e.message : String(e)}`);
			}
			followHubEvents();
		}),
		vscode.commands.registerCommand('aurix-debugger.hubAttach', async () => {
			const { url, token } = hubTarget();
			const client = new hub.HubClient({ url, token, clientId: 'vscode:attach' });
			const sessions = (await client.listSessions().catch(() => [])).filter((s) => s.state !== 'exited');
			if (sessions.length === 0) {
				void vscode.window.showInformationMessage(`No session is open on ${url}.`);
				return;
			}
			const pick = await vscode.window.showQuickPick(
				sessions.map((s) => ({ label: s.name, description: s.session, detail: `${s.targets.join(', ')}${s.opened_by ? ` - opened by ${s.opened_by}` : ''}`, s })),
				{ title: 'Attach the debugger to a hub session' },
			);
			if (pick) {
				await attachToHubSession(pick.s);
			}
		}),
		vscode.commands.registerCommand('aurix-debugger.hubAddMcpConfig', async () => {
			const folder = vscode.workspace.workspaceFolders?.[0];
			if (!folder) {
				void vscode.window.showErrorMessage('Open a folder first: the MCP configuration goes into its .mcp.json.');
				return;
			}
			const { url } = hubTarget();
			const choice = await vscode.window.showQuickPick(
				[
					{ label: 'stdio', description: 'Claude starts the hub when needed (aurix-hub stdio)', value: 'stdio' },
					{ label: 'http', description: `${url}/mcp (the hub runs at logon or already)`, value: 'http' },
				],
				{ title: 'How should Claude reach the debugger hub?' },
			);
			if (!choice) {
				return;
			}
			const file = path.join(folder.uri.fsPath, '.mcp.json');
			let doc: any = {};
			try {
				doc = JSON.parse(fs.readFileSync(file, 'utf8'));
			} catch {
				doc = {};
			}
			doc.mcpServers = doc.mcpServers ?? {};
			doc.mcpServers['aurix-debugger'] = choice.value === 'http'
				? { type: 'http', url: `${url}/mcp` }
				: { command: hub.installedHubExe(), args: ['stdio'] };
			const ok = await vscode.window.showInformationMessage(`Write the "aurix-debugger" MCP server (${choice.label}) into ${file}?`, { modal: true }, 'Write');
			if (ok === 'Write') {
				fs.writeFileSync(file, JSON.stringify(doc, null, '\t') + '\n');
				void vscode.window.showTextDocument(vscode.Uri.file(file));
			}
		}),
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (e.affectsConfiguration('aurix-debugger.hub')) {
				followHubEvents();
			}
		}),
	);
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
	const cfg = vscode.workspace.getConfiguration('aurix-debugger');
	// A remembered debug layout puts the view back into its group; else it goes to the right.
	const savedColumn = debugLayout?.takeDisassemblyColumn();
	const autoOpen = cfg.get<boolean>('autoOpenDisassemblyView', true);
	if (!autoOpen && savedColumn === undefined) {
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

	const placed = savedColumn !== undefined && await focusEditorGroup(savedColumn);
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

	if (!placed) {
		try {
			await vscode.commands.executeCommand('workbench.action.moveEditorToRightGroup');
		} catch (e) {
			agentOutput?.appendLine(`Failed to move disassembly view to the right: ${String(e)}`);
		}
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

	// A restored layout has its own group sizes.
	if (autoEven && !placed) {
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
	return pickSearchedVariableOrText({
		title: 'Add Live Watch Expression',
		placeholder: 'Pick a variable or type an expression, e.g. counter_6 or *(uint32_t*)0x70000044',
		typedDescription: 'Add expression',
		session: session?.type === 'aurixdbg' ? session : undefined,
	});
}

/** Rows a search shows: the server ranks, so the best are among them. */
const VARIABLE_SEARCH_LIMIT = 300;
/** Typing pauses this long before the server is asked, so fast typing asks once. */
const VARIABLE_SEARCH_DELAY_MS = 40;

type VariableItem = vscode.QuickPickItem & { expression?: string };

/**
 * Quick pick of the program's variables and struct members that also accepts any typed text.
 * The server searches as the user types (`aurix/searchVariables`) and sends the best matches
 * only, so the list stays short however large the program is. The typed text is offered first
 * unless a variable has exactly that name.
 */
function pickSearchedVariableOrText(options: {
	title: string;
	placeholder: string;
	typedDescription: string;
	session: vscode.DebugSession | undefined;
}): Promise<string | undefined> {
	const qp = vscode.window.createQuickPick<VariableItem>();
	qp.title = options.title;
	qp.placeholder = options.placeholder;
	qp.matchOnDescription = true;
	// The text the shown items were searched for, and the number of the latest request.
	let shownFor: string | undefined;
	let latest = 0;
	let timer: NodeJS.Timeout | undefined;
	const show = (text: string, total: number, hits: Array<{ name: string; expression: string; address?: number; size: number; file?: string }>) => {
		// The server matched already: alwaysShow keeps the quick pick from filtering them again.
		const items: VariableItem[] = hits.map((h) => ({
			label: h.name,
			description: [
				h.address !== undefined ? `0x${h.address.toString(16)}` : undefined,
				h.size ? `${h.size} byte${h.size === 1 ? '' : 's'}` : undefined,
				h.file ? path.basename(h.file.replace(/\\/g, '/')) : undefined,
			].filter(Boolean).join(' · '),
			alwaysShow: true,
			expression: h.expression,
		}));
		const exact = items.some((i) => i.label === text || i.expression === text);
		const list: VariableItem[] = text && !exact ? [{ label: text, description: options.typedDescription, alwaysShow: true }, ...items] : items;
		if (total > hits.length) {
			list.push({ label: `${total - hits.length} more: keep typing to narrow`, kind: vscode.QuickPickItemKind.Separator });
		}
		qp.items = list;
		shownFor = text;
	};
	const search = () => {
		timer = undefined;
		const text = qp.value.trim();
		if (!options.session) {
			show(text, 0, []);
			return;
		}
		const mine = ++latest;
		qp.busy = true;
		void Promise.resolve(options.session.customRequest('aurix/searchVariables', { text, limit: VARIABLE_SEARCH_LIMIT }))
			.then((r) => {
				if (mine === latest) {
					show(text, typeof r?.total === 'number' ? r.total : 0, Array.isArray(r?.items) ? r.items : []);
				}
			})
			.catch(() => {
				if (mine === latest) {
					show(text, 0, []);
				}
			})
			.finally(() => {
				if (mine === latest) {
					qp.busy = false;
				}
			});
	};
	return new Promise((resolve) => {
		qp.onDidChangeValue(() => {
			if (timer) {
				clearTimeout(timer);
			}
			timer = setTimeout(search, VARIABLE_SEARCH_DELAY_MS);
		});
		qp.onDidAccept(() => {
			const text = qp.value.trim();
			const picked = qp.selectedItems[0];
			// Accepted before the list caught up with the text: the text is what the user means.
			resolve(picked && shownFor === text ? picked.expression ?? picked.label : text || undefined);
			qp.hide();
		});
		qp.onDidHide(() => {
			if (timer) {
				clearTimeout(timer);
			}
			latest++;
			resolve(undefined);
			qp.dispose();
		});
		qp.show();
		search();
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
				const symbols: Array<{ name: string; address: string; size?: number; file?: string }> = Array.isArray(r?.symbols) ? r.symbols : [];
				symbolItems = symbols.map((sym) => ({
					label: sym.name,
					description: [
						sym.address,
						sym.size ? `${sym.size} byte${sym.size === 1 ? '' : 's'}` : undefined,
						sym.file ? path.basename(sym.file.replace(/\\/g, '/')) : undefined,
					].filter(Boolean).join(' · '),
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
	const cfg = vscode.workspace.getConfiguration('aurix-debugger');
	return cfg.get<boolean>('instructionSteppingDefault', false);
}

function getDisassemblyOpcodeBytes(): DisassemblyOpcodeBytes {
	const value = vscode.workspace.getConfiguration('aurix-debugger').get('disassembly.opcodeBytes');
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
	const cfg = vscode.workspace.getConfiguration('aurix-debugger');
	return normalizeLiveWatchIntervalMs(cfg.get<number>('liveWatch.intervalMs', 1000));
}

/** Stores the Live Watch refresh interval and applies it while an AURIX session runs. */
async function setLiveWatchInterval(value: number): Promise<void> {
	lastLiveWatchIntervalMs = normalizeLiveWatchIntervalMs(value);
	await extensionContext.workspaceState.update(liveWatchIntervalStateKey, lastLiveWatchIntervalMs);
	const sessionActive = activeAurixSessions > 0;
	updateLiveWatchStatus(sessionActive);
	// One rate for every live view, so there is a single thing to reason about.
	setLiveUpdateIntervalMs(lastLiveWatchIntervalMs);
	setLiveUpdateSessionActive(sessionActive);
}

function liveWatchPausesWhileHalted(): boolean {
	return vscode.workspace.getConfiguration('aurix-debugger').get<boolean>('liveWatch.pauseWhileHalted', true);
}

function applyLiveWatchPauseWhileHalted(): void {
	setLiveUpdatePauseWhileHalted(liveWatchPausesWhileHalted());
	updateLiveWatchStatus(activeAurixSessions > 0);
}

function updateLiveWatchStatus(visible: boolean): void {
	if (!liveWatchStatusItem) {
		return;
	}
	liveWatchStatusItem.text = `Live: ${lastLiveWatchIntervalMs}ms`;
	const overrides = liveWatches?.hasIntervalOverride()
		? ' Live Watches with their own interval (set with their gear button) keep it.'
		: '';
	const halted = liveWatchPausesWhileHalted()
		? ' While every TriCore core is halted they only refresh once on the halt (setting liveWatch.pauseWhileHalted).'
		: ' They also refresh while the target is halted (setting liveWatch.pauseWhileHalted).';
	liveWatchStatusItem.tooltip = `The live views refresh every ${lastLiveWatchIntervalMs} ms while the target runs.${halted}${overrides} A view that is not on screen reads nothing. Click to change.`;
	if (visible) {
		liveWatchStatusItem.show();
	} else {
		liveWatchStatusItem.hide();
	}
}

/** Shows the health of the session that reported last; hidden when it is ok or unknown. */
function updateTargetHealthItem(): void {
	if (!targetHealthItem) {
		return;
	}
	const latest = [...targetHealthBySession.values()].pop();
	const status = targetHealthStatus(latest);
	if (!status) {
		targetHealthItem.hide();
		return;
	}
	targetHealthItem.text = status.text;
	targetHealthItem.tooltip = status.tooltip;
	targetHealthItem.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground');
	targetHealthItem.show();
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
	const line = typeof frame.line === 'number' && frame.line > 0 ? frame.line : 1;
	return autoRevealSourcePath(srcPath, line, item.session);
}

/** Returns true when the location is shown in an editor. */
async function autoRevealSourcePath(srcPath: string, line: number, session?: vscode.DebugSession, preserveFocus = false): Promise<boolean> {
	if (!fs.existsSync(srcPath)) {
		reportMissingSource(session ?? vscode.debug.activeDebugSession, srcPath, line);
		if (preserveFocus) {
			// Next to the focused Disassembly view VS Code opens no source itself: show its
			// "file not found" editor, so it is clear that the source could not be located.
			const viewColumn = preferredSourceViewColumn ?? vscode.ViewColumn.One;
			void Promise.resolve(vscode.commands.executeCommand('vscode.open', vscode.Uri.file(srcPath), { viewColumn, preview: true, preserveFocus: true }))
				.catch(() => undefined);
		}
		return false;
	}
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
				preserveFocus
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
				preserveFocus
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

/** Learned source path mappings, kept per workspace. */
function sourceMappingStore(): SourceMappingStore | undefined {
	const state = extensionContext?.workspaceState;
	if (!state) {
		return undefined;
	}
	return {
		load: () => state.get<SourcePrefixMapping[]>(LEARNED_SOURCE_MAPPINGS_KEY, []),
		save: (mappings) => void state.update(LEARNED_SOURCE_MAPPINGS_KEY, mappings)
	};
}

/**
 * A stop location whose source file does not exist locally (e.g. an object a compiler cache built in
 * another checkout): logs it once per session and offers once per session to locate the file.
 */
function reportMissingSource(session: vscode.DebugSession | undefined, srcPath: string, line: number): void {
	if (session?.type !== 'aurixdbg') {
		return;
	}
	let logged = missingSourcesLogged.get(session.id);
	if (!logged) {
		logged = new Set<string>();
		missingSourcesLogged.set(session.id, logged);
	}
	const key = normalizePathForCompare(srcPath);
	if (logged.has(key)) {
		return;
	}
	logged.add(key);
	agentOutput?.appendLine(
		`Source not found: ${srcPath}. Map the folder of the build onto your checkout with "sourceFileMap" ` +
		`(e.g. "sourceFileMap": { "<build folder>": "\${workspaceFolder}" }) or add your checkout to "sourceSearchPaths".`
	);
	if (missingSourcePromptShown.has(session.id)) {
		return;
	}
	missingSourcePromptShown.add(session.id);
	void (async () => {
		const locate = 'Locate File…';
		const picked = await vscode.window.showWarningMessage(
			`Source file not found: ${srcPath}. Locate it in your checkout to map the other files of that build as well.`,
			locate
		);
		if (picked !== locate) {
			return;
		}
		const name = path.basename(srcPath.replace(/\\/g, '/'));
		const files = await vscode.window.showOpenDialog({
			canSelectMany: false,
			defaultUri: vscode.workspace.workspaceFolders?.[0]?.uri,
			openLabel: `Use as ${name}`,
			title: `Locate ${name}`
		});
		const localPath = files?.[0]?.fsPath;
		if (!localPath) {
			return;
		}
		try {
			const mapping = await session.customRequest('aurix/addSourceMapping', { debugPath: srcPath, localPath }) as SourcePrefixMapping;
			agentOutput?.appendLine(`Source mapping added: ${mapping.from} -> ${mapping.to}`);
			// The other missing files of this session may resolve now.
			missingSourcesLogged.delete(session.id);
			missingSourcePromptShown.delete(session.id);
			await autoRevealSourcePath(localPath, line, session);
		} catch (e) {
			void vscode.window.showErrorMessage(`Cannot map ${name}: ${e instanceof Error ? e.message : String(e)}`);
		}
	})();
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


export async function deactivate(): Promise<void> {
	// VS Code closes (or the window reloads): the sessions this window opened with sessionLifetime
	// "close" are closed, the others are left (this client's breakpoints and lease go). The hub
	// outlives VS Code. A local hub also closes them by itself once this process is gone
	// (owner_pid), which covers a VS Code that crashed or was killed.
	stopHubEvents?.();
	for (const id of [...hubBindings.keys()]) {
		void endHubBinding(id);
	}
	if (pendingHubEnds.size) {
		// VS Code gives deactivate a few seconds; the hub finishes a close it has received anyway.
		await Promise.race([
			Promise.allSettled([...pendingHubEnds]),
			new Promise((r) => setTimeout(r, 4000)),
		]);
	}
	agentOutput?.dispose();
	agentOutput = undefined;
}
