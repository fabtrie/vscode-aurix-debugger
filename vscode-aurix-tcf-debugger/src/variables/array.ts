/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import { GetContextSymbolsCommand, TCFSymbolContextData, TCFTypeClass } from "../tcf-all";
import { AbstractVariable, ClientVariable, NameProvider, RawValueProvider, VariableHelper } from "./types";

function isCharLikeType(type: TCFSymbolContextData): boolean {
    // By name: arrays of uint8 are byte arrays, not strings.
    const normalizedName = (type.Name ?? '').trim().toLowerCase();
    return normalizedName.includes('char')
    && (type.TypeClass === TCFTypeClass.cardinal || type.TypeClass === TCFTypeClass.integer);
}

function escapeChar(byte: number): string {
    switch (byte) {
        case 0x00:
            return '\\0';
        case 0x07:
            return '\\a';
        case 0x08:
            return '\\b';
        case 0x09:
            return '\\t';
        case 0x0A:
            return '\\n';
        case 0x0B:
            return '\\v';
        case 0x0C:
            return '\\f';
        case 0x0D:
            return '\\r';
        case 0x22:
            return '\\"';
        case 0x5C:
            return '\\\\';
        default:
            if (byte >= 0x20 && byte <= 0x7E) {
                return String.fromCharCode(byte);
            }
            return `\\x${byte.toString(16).padStart(2, '0')}`;
    }
}

function formatCString(rawValue: Buffer): string {
    const zeroIndex = rawValue.indexOf(0);
    const end = zeroIndex >= 0 ? zeroIndex : rawValue.length;
    let result = '';
    for (let i = 0; i < end; i++) {
        result += escapeChar(rawValue[i]);
    }
    return `"${result}"`;
}

export class ArrayVariable extends AbstractVariable {

    private children: ClientVariable[] | undefined = undefined;
    private baseTypeDetails: TCFSymbolContextData | undefined = undefined;

    helper: VariableHelper;

    constructor(type: TCFSymbolContextData, helper: VariableHelper, nameProvider: NameProvider, valueProvider: RawValueProvider) {
        super(type, undefined, nameProvider, valueProvider);
        this.helper = helper;
    }
    async getChildren(): Promise<ClientVariable[]> {
        if (this.children === undefined) {
            await this.loadChildren();
        }
        return this.children ?? [];
    }

    private async getBaseTypeDetails(): Promise<TCFSymbolContextData | undefined> {
        if (!this.type.BaseTypeID) {
            return undefined;
        }
        if (!this.baseTypeDetails) {
            this.baseTypeDetails = await this.helper.sendCommand(new GetContextSymbolsCommand(this.type.BaseTypeID));
        }
        return this.baseTypeDetails;
    }

    async loadChildren() {
        const baseTypeDetails = await this.getBaseTypeDetails();
        if (!baseTypeDetails) {
            return; //no known element type?
        }

        const symbolDetails = this.type; //TODO: Or do we need symbolDetails?
        const size = symbolDetails.Size;
        const length = symbolDetails.Length; //TODO: This has to be done lazily otherwise large arrays will take all the RAM
        if (!size || !length) {
            return; //TODO: log?
        }
        const elementSize = size / length;

        let children = [];
        for (let i = 0; i < length; i++) {
            const self = this;
            const valueProvider = new class implements RawValueProvider {
                async value(): Promise<Buffer | undefined> {
                    const value = await self.valueProvider.value();
                    const elementBytes = value?.subarray(i * elementSize, (i + 1) * elementSize);
                    return elementBytes;
                }
                isBigEndian(): boolean {
                    return self.valueProvider.isBigEndian();
                }
            }();

            const nameProvider = new class implements NameProvider {
                name() {
                    return `[${i}]`;
                }
            }();

            const v = this.helper.createVariable(baseTypeDetails, undefined, nameProvider, valueProvider); //TODO: do array elements even have symbols?

            if (v !== undefined) {
                children.push(v);
            }
        }
        this.children = children;
    }

    async displayValue(): Promise<string | undefined> {
        const baseTypeDetails = await this.getBaseTypeDetails();
        const rawValue = await this.valueProvider.value();
        if (baseTypeDetails && rawValue && isCharLikeType(baseTypeDetails)) {
            return formatCString(rawValue);
        }
        return "[...]"; //TODO: What should an array display for value? First N elements?
    }
}
