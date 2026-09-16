/*
 * Aurix TCF Debugger VSCode Extension
 * Copyright (C) 2025 Infineon Technologies AG
 *
 * Debugger actions: user-defined Tcl commands from the launch configuration ("actions"),
 * offered through a single "Debugger Actions" toolbar button as a (nested) QuickPick.
 * VS Code menus are static, so the actions cannot become toolbar buttons of their own.
 */
import * as vscode from 'vscode';

/** One entry of the launch configuration "actions" array: a group (items) or an action (tcl). */
export interface DebuggerActionNode {
	id?: string;
	label: string;
	description?: string;
	detail?: string;
	/** Codicon name without "$(...)", e.g. "trash". */
	icon?: string;
	/** Group: nested entries, shown as a submenu. */
	items?: DebuggerActionNode[];
	/** Action: Tcl script(s) run in order on the debug server; stops at the first error. */
	tcl?: string | string[];
	/** Modal confirmation text shown before running. */
	confirm?: string;
	/** Reset the target afterwards like the Reset toolbar button. */
	resetAfter?: boolean;
	/** Show the Tcl output in a notification (it always goes to the Debug Console). */
	showOutput?: boolean;
	/** Command timeout per script in seconds. */
	timeout?: number;
}

const HAS_ACTIONS_CONTEXT_KEY = 'aurix-tcf.hasActions';
const DEFAULT_ACTION_TIMEOUT_S = 900;

let running = false;

type ActionPick = vscode.QuickPickItem & { node: DebuggerActionNode };

export function registerDebuggerActions(context: vscode.ExtensionContext): void {
	context.subscriptions.push(
		vscode.commands.registerCommand('aurix-tcf.showActions', async () => {
			const session = getAurixSession();
			if (!session) {
				return;
			}
			const action = await pickAction(getActions(session));
			if (action) {
				await runAction(session, action);
			}
		}),
		vscode.commands.registerCommand('aurix-tcf.runAction', async (args?: { id?: string }) => {
			const session = getAurixSession();
			if (!session) {
				return;
			}
			if (typeof args?.id !== 'string') {
				await vscode.commands.executeCommand('aurix-tcf.showActions');
				return;
			}
			const action = findAction(getActions(session), args.id);
			if (!action) {
				void vscode.window.showErrorMessage(`AURIX™ TCF: no action with id "${args.id}" in the launch configuration.`);
				return;
			}
			await runAction(session, action);
		}),
		vscode.debug.onDidChangeActiveDebugSession(updateActionsContext),
		vscode.debug.onDidStartDebugSession(() => updateActionsContext()),
	);
	updateActionsContext();
}

function updateActionsContext(): void {
	const session = vscode.debug.activeDebugSession;
	const hasActions = !!session && session.type === 'aurix-tcf' && getActions(session).length > 0;
	void vscode.commands.executeCommand('setContext', HAS_ACTIONS_CONTEXT_KEY, hasActions);
}

function getAurixSession(): vscode.DebugSession | undefined {
	const s = vscode.debug.activeDebugSession;
	if (!s || s.type !== 'aurix-tcf') {
		void vscode.window.showWarningMessage('No active AURIX™ TCF debug session.');
		return undefined;
	}
	return s;
}

function getActions(session: vscode.DebugSession): DebuggerActionNode[] {
	const actions = session.configuration.actions;
	return Array.isArray(actions) ? actions.filter(isNode) : [];
}

function isNode(n: unknown): n is DebuggerActionNode {
	return typeof n === 'object' && n !== null && typeof (n as DebuggerActionNode).label === 'string';
}

function childNodes(node: DebuggerActionNode): DebuggerActionNode[] | undefined {
	return Array.isArray(node.items) ? node.items.filter(isNode) : undefined;
}

function findAction(nodes: DebuggerActionNode[], id: string): DebuggerActionNode | undefined {
	for (const node of nodes) {
		const children = childNodes(node);
		const found = children ? findAction(children, id) : node.id === id ? node : undefined;
		if (found) {
			return found;
		}
	}
	return undefined;
}

/** Walks the action tree with one QuickPick per level; groups open a sub level with a Back button. */
async function pickAction(nodes: DebuggerActionNode[]): Promise<DebuggerActionNode | undefined> {
	if (nodes.length === 0) {
		void vscode.window.showInformationMessage('AURIX™ TCF: no "actions" in the launch configuration.');
		return undefined;
	}
	// A single top-level group opens directly.
	const single = nodes.length === 1 ? childNodes(nodes[0]) : undefined;
	const stack = single
		? [{ title: `Debugger Actions › ${nodes[0].label}`, nodes: single }]
		: [{ title: 'Debugger Actions', nodes }];
	while (stack.length > 0) {
		const level = stack[stack.length - 1];
		const picked = await showLevel(level.title, level.nodes, stack.length > 1);
		if (picked === 'back') {
			stack.pop();
			continue;
		}
		if (!picked) {
			return undefined;
		}
		const children = childNodes(picked);
		if (children) {
			stack.push({ title: `${level.title} › ${picked.label}`, nodes: children });
			continue;
		}
		return picked;
	}
	return undefined;
}

function showLevel(title: string, nodes: DebuggerActionNode[], canGoBack: boolean): Promise<DebuggerActionNode | 'back' | undefined> {
	return new Promise(resolve => {
		const qp = vscode.window.createQuickPick<ActionPick>();
		qp.title = title;
		qp.placeholder = 'Select a debugger action';
		qp.buttons = canGoBack ? [vscode.QuickInputButtons.Back] : [];
		qp.items = nodes.map(node => ({
			label: `${node.icon ? `$(${node.icon}) ` : ''}${node.label}${childNodes(node) ? ' $(chevron-right)' : ''}`,
			description: node.description,
			detail: node.detail,
			node,
		}));
		let result: DebuggerActionNode | 'back' | undefined;
		qp.onDidTriggerButton(button => {
			if (button === vscode.QuickInputButtons.Back) {
				result = 'back';
				qp.hide();
			}
		});
		qp.onDidAccept(() => {
			result = qp.selectedItems[0]?.node;
			qp.hide();
		});
		qp.onDidHide(() => {
			qp.dispose();
			resolve(result);
		});
		qp.show();
	});
}

async function runAction(session: vscode.DebugSession, action: DebuggerActionNode): Promise<void> {
	const scripts = (Array.isArray(action.tcl) ? action.tcl : [action.tcl])
		.filter((s): s is string => typeof s === 'string' && s.trim() !== '');
	if (scripts.length === 0) {
		void vscode.window.showErrorMessage(`AURIX™ TCF: action "${action.label}" has no "tcl" command.`);
		return;
	}
	if (action.confirm) {
		const choice = await vscode.window.showWarningMessage(action.confirm, { modal: true }, action.label);
		if (choice !== action.label) {
			return;
		}
	}
	if (running) {
		void vscode.window.showWarningMessage('AURIX™ TCF: another debugger action is still running.');
		return;
	}
	running = true;
	try {
		const timeoutMs = (typeof action.timeout === 'number' && action.timeout > 0 ? action.timeout : DEFAULT_ACTION_TIMEOUT_S) * 1000;
		const outputs: string[] = [];
		await vscode.window.withProgress(
			{ location: vscode.ProgressLocation.Notification, title: `AURIX™ TCF: ${action.label} (see Debug Console)...` },
			async () => {
				for (const script of scripts) {
					const res = await session.customRequest('aurix-tcf/tcl', { script, timeoutMs });
					if (typeof res?.result === 'string' && res.result.trim() !== '') {
						outputs.push(res.result.trim());
					}
				}
				if (action.resetAfter) {
					await session.customRequest('aurix-tcf/reset', {});
				}
			}
		);
		const output = action.showOutput && outputs.length > 0 ? `: ${outputs.join('\n')}` : ' done.';
		void vscode.window.showInformationMessage(`AURIX™ TCF: ${action.label}${output}`);
	} catch (e) {
		void vscode.window.showErrorMessage(`AURIX™ TCF: ${action.label} failed: ${e instanceof Error ? e.message : String(e)}`);
	} finally {
		running = false;
	}
}
