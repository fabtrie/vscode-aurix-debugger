import { PromiseError, PromiseSuccess, SimpleCommand, asString, parseResponse } from './tcfutils';

export const TCL_SERVICE = 'Tcl';

/**
 * aurix-dbg custom service: evaluates an OpenOCD-style Tcl command on the
 * debug server (the equivalent of gdb's "monitor" command). The result is the
 * command output as a string.
 */
export class TclEvalCommand extends SimpleCommand<string> {
	constructor(private readonly script: string) {
		super();
	}

	service(): string {
		return TCL_SERVICE;
	}

	command(): string {
		return 'eval';
	}

	debugDescription(tokenID: number): string {
		return `${this.service()}/${tokenID}/eval`;
	}

	arguments() {
		return this.script;
	}

	result(responseAll: Buffer[], success: PromiseSuccess<string>, error: PromiseError<string>): void {
		parseResponse(responseAll, success, error, asString);
	}
}
