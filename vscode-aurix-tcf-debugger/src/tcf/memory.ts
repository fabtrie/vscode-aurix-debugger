/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import { PromiseError, PromiseSuccess, SimpleCommand, ValidatingCommand, asNullable, asString, asStringNullableArray, handleError, handleErrorBuffer, responseLengthAbout, toBuffer, validateJSON, JSONValidationError } from "./tcfutils";
import * as validateMemoryResult from './validators/validate-MemoryResult';

abstract class MemoryCommand<T> extends SimpleCommand<T> {
    constructor() {
        super();
    }

    service(): string {
        return "Memory";
    }

    debugDescription(tokenID: number): string {
        return `${this.service()}/${tokenID}`;
    }

}

abstract class MemoryValidatingCommand<T> extends ValidatingCommand<T> {
    constructor() {
        super();
    }

    service(): string {
        return "Memory";
    }

    debugDescription(tokenID: number): string {
        return `${this.service()}/${tokenID}`;
    }

}


//see https://download.eclipse.org/tools/tcf/tcf-docs/TCF%20Service%20-%20Memory.html#CmdGetChildren
export class GetChildrenMemoryCommand extends MemoryValidatingCommand<string[] | null> {
    parentContextID: string | null;

    constructor(parentContextID: string | null) {
        super();
        this.parentContextID = parentContextID;
    }

    command(): string {
        return "getChildren";
    }

    arguments() {
        return this.parentContextID;
    }

    toBuffer(token: string): Buffer {
        // Explicitly serialize null for root queries.
        return toBuffer(["C", token, this.service(), this.command(), JSON.stringify(this.arguments())]);
    }

    override cast(json: any): string[] | null {
        return asStringNullableArray(json);
    }
}

export interface MemoryContextData {
    ID: string;
    ParentID?: string | null;
    Name?: string;
    Address?: number;
    Size?: number;
    [key: string]: any;
}

export function asMemoryContextData(result: any): MemoryContextData {
    if (!result || typeof result !== "object") {
        throw new JSONValidationError("Not an object", result);
    }
    if (!("ID" in result)) {
        throw new JSONValidationError("Missing ID", result);
    }
    asString((result as any).ID);
    return result as MemoryContextData;
}

export class GetContextMemoryCommand extends MemoryValidatingCommand<MemoryContextData | null> {
    contextID: string;

    constructor(contextID: string) {
        super();
        this.contextID = contextID;
    }

    command(): string {
        return "getContext";
    }

    arguments() {
        return this.contextID;
    }

    override cast(json: any): MemoryContextData | null {
        return asNullable(json, asMemoryContextData);
    }
}

export interface MemoryErrorAddress {
    addr: number,
    size: number,
    stat: number,
    msg: any
}

export interface MemoryResult {
    base64: string,
    errorAddresses: MemoryErrorAddress[] | null
}

export function asMemoryResult(result: any): MemoryResult {
    return validateJSON(result, validateMemoryResult) as MemoryResult; //type assertion OK
}

//see https://download.eclipse.org/tools/tcf/tcf-docs/TCF%20Service%20-%20Memory.html#CmdGetMemory
export class GetMemoryCommand extends MemoryCommand<MemoryResult> {
    contextID: string;
    address: number;
    wordSize: number;
    byteCount: number;
    mode: number;

    constructor(contextID: string, address: number, wordSize: number, byteCount: number, mode: number = 0) {
        super();
        this.contextID = contextID;
        this.address = address;
        this.wordSize = wordSize;
        this.byteCount = byteCount;
        this.mode = mode;
    }

    command(): string {
        return "get";
    }

    arguments() {
        return undefined;
    }

    toBuffer(token: string): Buffer {
        return toBuffer(["C", token, this.service(), this.command(),
            JSON.stringify(this.contextID), JSON.stringify(this.address), JSON.stringify(this.wordSize),
            JSON.stringify(this.byteCount), JSON.stringify(this.mode)], undefined);
    }

    result(responseAll: Buffer[], success: PromiseSuccess<MemoryResult>, error: PromiseError<MemoryResult>): void {
        parseMemoryResponse(responseAll, success, error);
    }
}

export function parseMemoryResponse(responseAll: Buffer[], success: PromiseSuccess<MemoryResult>, error: PromiseError<MemoryResult>): void {
    const [response, resultError, errorAddresses, ...etc] = responseAll;

    if (handleErrorBuffer(resultError, error)) {
        return;
    }

    if (!responseLengthAbout(0, etc)) {
        console.log(`Response should have a single JSON, got ${response.length} buffers`);
    }

    try {
        success(asMemoryResult({
            base64: asString(JSON.parse(response.toString())),
            errorAddresses: JSON.parse(errorAddresses.toString()) as MemoryErrorAddress[] //type assertion OK
        })); //base64 string
    } catch (e) {
        handleError(e, error);
    }
}

export class SetMemoryCommand extends MemoryCommand<MemoryErrorAddress[] | null> {
    contextID: string;
    address: number;
    wordSize: number;
    byteCount: number;
    valueBase64: string;
    mode: number;

    constructor(contextID: string, address: number, wordSize: number, byteCount: number, valueBase64: string, mode: number = 0) {
        super();
        this.contextID = contextID;
        this.address = address;
        this.wordSize = wordSize;
        this.byteCount = byteCount;
        this.valueBase64 = valueBase64;
        this.mode = mode;
    }

    command(): string {
        return "set";
    }

    arguments() {
        return undefined;
    }

    toBuffer(token: string): Buffer {
        return toBuffer([
            "C",
            token,
            this.service(),
            this.command(),
            JSON.stringify(this.contextID),
            JSON.stringify(this.address),
            JSON.stringify(this.wordSize),
            JSON.stringify(this.byteCount),
            JSON.stringify(this.mode),
            JSON.stringify(this.valueBase64),
        ], undefined);
    }

    result(responseAll: Buffer[], success: PromiseSuccess<MemoryErrorAddress[] | null>, error: PromiseError<MemoryErrorAddress[] | null>): void {
        parseMemorySetResponse(responseAll, success, error);
    }
}

export function parseMemorySetResponse(
    responseAll: Buffer[],
    success: PromiseSuccess<MemoryErrorAddress[] | null>,
    error: PromiseError<MemoryErrorAddress[] | null>
): void {
    if (responseAll.length === 0) {
        error(new SyntaxError("Response should include error status"));
        return;
    }

    let resultError: Buffer | undefined;
    let errorAddresses: Buffer | undefined;

    if (responseAll.length >= 3) {
        // Consistent with Memory.get: [response, error, errorAddresses, ...]
        resultError = responseAll[1];
        errorAddresses = responseAll[2];
    } else if (responseAll.length === 2) {
        // Either [error, errorAddresses] or [response, error].
        const maybeErrorAddresses = responseAll[1];
        try {
            const json = JSON.parse(maybeErrorAddresses.toString());
            if (Array.isArray(json)) {
                resultError = responseAll[0];
                errorAddresses = maybeErrorAddresses;
            } else {
                resultError = responseAll[1];
            }
        } catch {
            resultError = responseAll[1];
        }
    } else {
        resultError = responseAll[0];
    }

    if (resultError && handleErrorBuffer(resultError, error)) {
        return;
    }

    if (!errorAddresses || errorAddresses.length === 0) {
        success(null);
        return;
    }

    try {
        success(JSON.parse(errorAddresses.toString()) as MemoryErrorAddress[]);
    } catch (e) {
        handleError(e, error);
    }
}