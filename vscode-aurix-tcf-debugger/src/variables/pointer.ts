/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import assert = require("assert");
import { FindByAddrSymbolsCommand, GetContextSymbolsCommand, TCFSymbolContextData, TCFTypeClass } from "../tcf-all";
import { AbstractVariable, ClientVariable, FixedNameProvider, NameProvider, RawValueProvider, UndefinedRawValueProvider, VariableHelper } from "./types";

export class PointerVariable extends AbstractVariable {
    private children: ClientVariable[] | undefined = undefined;
    helper: VariableHelper;

    _displayValue: string | undefined;
    loadedValue: boolean = false;

    constructor(type: TCFSymbolContextData, symbolDetails: TCFSymbolContextData | undefined, nameProvider: NameProvider, valueProvider: RawValueProvider, helper: VariableHelper) {
        super(type, symbolDetails, nameProvider, valueProvider);
        this.helper = helper;
    }

    async getChildren() {
        if (this.children === undefined) {
            await this.loadChildren();
        }
        return this.children ?? [];
    }

    async loadChildren() {
        if (this.type.BaseTypeID) {
            //XXX: it looks like we would need to recurse here, eg. we could have a pointer to a pointer to a struct, etc.
            const baseTypeDetails = await this.helper.sendCommand(new GetContextSymbolsCommand(this.type.BaseTypeID));
            // How to load the symbol for the pointed data?
            // It may not even be a symbol but just a memory region.
            // const symbolDetails = await this.helper.sendCommand(new GetContextSymbolsCommand(...)
            // or perhaps this.sendCommand(new EvaluateExpressionCommand(varName+"."+this.type.Name)) ?

            baseTypeDetails.Name = baseTypeDetails.Name + "*"; //TODO: marking this as a pointer type. Normally we should return the full list of types upstream and the client decides how to present it in the UI

            const v = this.helper.createVariable(baseTypeDetails, undefined /* no symbol data */, new FixedNameProvider("*"), new UndefinedRawValueProvider());
            if (v !== undefined) {
                this.children = [v];
            } else {
                this.children = []; //maybe display something generic? Like... "pointer to function"
            }
        } else {
            //TODO: log error?
            this.children = [];
        }
    }

    async displayValue(): Promise<string | undefined> {
        if (!this.loadedValue) {
            this.loadedValue = true;
            this._displayValue = await this.loadValue();
        }
        return this._displayValue;
    }

    async loadValue(): Promise<string | undefined> {
        const variableRawValue = await this.valueProvider.value();
        if (!variableRawValue) {
            return undefined;
        }

        assert(this.type.TypeClass === TCFTypeClass.pointer);
        const bigEndian = this.valueProvider.isBigEndian();
        let value: string;
        switch (variableRawValue.length) {
            case 8:
                value = '0x' + (bigEndian ? variableRawValue.readBigUInt64BE() : variableRawValue.readBigUInt64LE()).toString(16);
                break;
            case 4:
                value = '0x' + (bigEndian ? variableRawValue.readUInt32BE() : variableRawValue.readUInt32LE()).toString(16);
                break;
            case 2:
                value = '0x' + (bigEndian ? variableRawValue.readUInt16BE() : variableRawValue.readUInt16LE()).toString(16);
                break;
            case 1:
                value = '0x' + variableRawValue.readUInt8().toString(16);
                break;
            default:
                value = '0x' + variableRawValue.toString('hex');
                break;
        }
        //TODO: actually read memory and load that type?
        //const expr = "*(${" + typeDetails[0].ID + "})" + (variableRawValue.toString('hex'));
        //const v = new EvaluateExpressionCommand(expr);
        const target = await this.functionAt(Number(value));
        return target ? `${value} <${target}>` : value;
    }

    /** For a function pointer: the function at `address` (`name` or `name+0x10`). */
    private async functionAt(address: number): Promise<string | undefined> {
        if (!this.type.BaseTypeID || !address || !Number.isSafeInteger(address)) {
            return undefined;
        }
        try {
            const base = await this.helper.sendCommand(new GetContextSymbolsCommand(this.type.BaseTypeID));
            if (base.TypeClass !== TCFTypeClass.function) {
                return undefined;
            }
            const id = await this.helper.sendCommand(new FindByAddrSymbolsCommand('', address));
            const fn = id ? await this.helper.sendCommand(new GetContextSymbolsCommand(id)) : undefined;
            if (!fn?.Name) {
                return undefined;
            }
            const offset = fn.Address !== undefined ? address - fn.Address : 0;
            return offset > 0 ? `${fn.Name}+0x${offset.toString(16)}` : fn.Name;
        } catch {
            return undefined; // no code there
        }
    }

}
