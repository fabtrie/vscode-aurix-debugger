/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import { GetChildrenSymbolsCommand, GetContextSymbolsCommand, TCFSymbolContextData, TCFTypeClass } from "../tcf-all";
import { Logger } from "./helper";
import { AbstractVariable, ClientVariable, NameProvider, RawValueProvider, VariableHelper } from "./types";

type IntegerDisplayKind = 'signed' | 'unsigned';

function decodeInteger(rawValue: Buffer, bigEndian: boolean, signed: boolean): bigint {
    let result = 0n;
    const bytes = bigEndian ? rawValue : Buffer.from(rawValue).reverse();

    for (const byte of bytes) {
        result = (result << 8n) | BigInt(byte);
    }

    if (signed && rawValue.length > 0) {
        const bitCount = BigInt(rawValue.length * 8);
        const signMask = 1n << (bitCount - 1n);
        if ((result & signMask) !== 0n) {
            result -= 1n << bitCount;
        }
    }

    return result;
}

function normalizeIntegerBuffer(rawValue: Buffer | undefined): Buffer | undefined {
    if (!rawValue || rawValue.length === 0) {
        return undefined;
    }
    return rawValue;
}

export class EnumVariable extends AbstractVariable {
    helper: VariableHelper;
    logger: Logger;
    private loadedValue = false;
    private cachedDisplayValue: string | undefined;
    private underlyingType: TCFSymbolContextData | undefined;

    constructor(type: TCFSymbolContextData, symbolDetails: TCFSymbolContextData | undefined, helper: VariableHelper, nameProvider: NameProvider, valueProvider: RawValueProvider, logger: Logger) {
        super(type, symbolDetails, nameProvider, valueProvider);
        this.helper = helper;
        this.logger = logger;
    }

    async getChildren(): Promise<ClientVariable[]> {
        return [];
    }

    async displayValue(): Promise<string | undefined> {
        if (!this.loadedValue) {
            this.loadedValue = true;
            this.cachedDisplayValue = await this.loadValue();
        }
        return this.cachedDisplayValue;
    }

    private async getUnderlyingType(): Promise<TCFSymbolContextData | undefined> {
        if (this.underlyingType !== undefined || !this.type.BaseTypeID) {
            return this.underlyingType;
        }
        try {
            this.underlyingType = await this.helper.sendCommand(new GetContextSymbolsCommand(this.type.BaseTypeID));
        } catch (e) {
            this.logger.log(`Could not load enum base type for ${this.name()}: ${String(e)}`);
        }
        return this.underlyingType;
    }

    private async getDisplayKind(): Promise<IntegerDisplayKind> {
        const underlyingType = await this.getUnderlyingType();
        if (underlyingType?.TypeClass === TCFTypeClass.cardinal) {
            return 'unsigned';
        }
        return 'signed';
    }

    private async formatRawValue(rawValue: Buffer): Promise<string> {
        const bigEndian = this.valueProvider.isBigEndian();
        const displayKind = await this.getDisplayKind();
        return String(decodeInteger(rawValue, bigEndian, displayKind === 'signed'));
    }

    private async loadValue(): Promise<string | undefined> {
        const rawValue = normalizeIntegerBuffer(await this.valueProvider.value());
        if (!rawValue) {
            return undefined;
        }

        const bigEndian = this.valueProvider.isBigEndian();
        const displayKind = await this.getDisplayKind();
        const currentValue = decodeInteger(rawValue, bigEndian, displayKind === 'signed');

        try {
            const children = await this.helper.sendCommand(new GetChildrenSymbolsCommand(this.type.ID)) || [];
            for (const childId of children) {
                const child = await this.helper.sendCommand(new GetContextSymbolsCommand(childId));
                const childRawValue = normalizeIntegerBuffer(child.Value ? Buffer.from(Buffer.from(child.Value).toString(), "base64") : undefined);
                if (!childRawValue || !child.Name) {
                    continue;
                }

                const childValue = decodeInteger(childRawValue, bigEndian, displayKind === 'signed');
                if (childValue === currentValue) {
                    return child.Name;
                }
            }
        } catch (e) {
            this.logger.log(`Could not resolve enum name for ${this.name()}: ${String(e)}`);
        }

        return this.formatRawValue(rawValue);
    }
}