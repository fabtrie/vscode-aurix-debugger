/*
SPDX-License-Identifier: MIT
*/
import { parseEmptyResponse, PromiseError, PromiseSuccess, SimpleCommand, toBuffer } from './tcfutils';

abstract class MemoryMapCommand<T> extends SimpleCommand<T> {
    constructor() {
        super();
    }

    service(): string {
        return 'MemoryMap';
    }

    debugDescription(tokenID: number): string {
        return `${this.service()}/${tokenID}`;
    }
}

/**
 * Minimal subset of the TCF MemoryMap region shape.
 * See: TCF Service - MemoryMap
 */
/* eslint-disable @typescript-eslint/naming-convention */
export interface MemoryMapRegion {
    Addr?: number;
    Size?: number;
    Offs?: number;
    FileName?: string;
    SectionName?: string;
    Flags?: number;
    BSS?: boolean;
    ContextQuery?: string;
    ID?: string;
    // Plus any additional attributes
    [key: string]: any;
}
/* eslint-enable */

abstract class MemoryMapEmptyCommand extends MemoryMapCommand<undefined> {
    override result(responseAll: Buffer[], success: PromiseSuccess<undefined>, error: PromiseError<undefined>): void {
        parseEmptyResponse(responseAll, success, error);
    }
}

// https://download.eclipse.org/tools/tcf/tcf-docs/TCF%20Service%20-%20MemoryMap.html#CmdSet
export class SetMemoryMapCommand extends MemoryMapEmptyCommand {
    private contextId: string;
    private map: MemoryMapRegion[];

    constructor(contextId: string, map: MemoryMapRegion[]) {
        super();
        this.contextId = contextId;
        this.map = map;
    }

    debugDescription(tokenID: number): string {
        return super.debugDescription(tokenID) + `/${this.command()}/${this.contextId || '<all>'}`;
    }

    command(): string {
        return 'set';
    }

    arguments() {
        // Not used: this command overrides toBuffer() because it has multiple args.
        return undefined;
    }

    override toBuffer(token: string): Buffer {
        // MemoryMap.set takes 2 args: context ID and the region array.
        return toBuffer([
            'C',
            token,
            this.service(),
            this.command(),
            JSON.stringify(this.contextId),
            JSON.stringify(this.map),
        ]);
    }
}
