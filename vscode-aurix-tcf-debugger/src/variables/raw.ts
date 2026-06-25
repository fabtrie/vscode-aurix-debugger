/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import { TCFSymbolContextData } from "../tcf-all";
import { AbstractVariable, ClientVariable, NameProvider, RawValueProvider } from "./types";

export class RawVariable extends AbstractVariable {
    children?: ClientVariable[];

    constructor(type: TCFSymbolContextData, symbolDetails: TCFSymbolContextData | undefined, children: ClientVariable[], nameProvider: NameProvider, valueProvider: RawValueProvider) {
        super(type, symbolDetails, nameProvider, valueProvider);
        this.children = children;
    }

    async displayValue(): Promise<string | undefined> {
        const rawValue = await this.valueProvider.value();
        if (!rawValue || rawValue.length === 0) {
            return undefined;
        }

        const truncated = rawValue.length > 32;
        const shown = truncated ? rawValue.subarray(0, 32) : rawValue;
        return `0x${shown.toString('hex')}${truncated ? '…' : ''}`;
    }

    async getChildren(): Promise<ClientVariable[]> {
        return this.children ?? [];
    }

}

