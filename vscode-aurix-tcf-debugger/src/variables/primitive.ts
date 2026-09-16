/*
Copyright (C) 2023 Intel Corporation
SPDX-License-Identifier: MIT
*/
import { TCFSymbolContextData, TCFTypeClass } from "../tcf-all";
import { Logger } from "./helper";
import { AbstractVariable, ClientVariable, NameProvider, RawValueProvider } from "./types";

function isCharLikeType(type: TCFSymbolContextData): boolean {
    // By name: 1-byte integer typedefs such as uint8/boolean are numbers, not characters.
    const normalizedName = (type.Name ?? '').trim().toLowerCase();
    return normalizedName.includes('char')
        && (type.TypeClass === TCFTypeClass.integer || type.TypeClass === TCFTypeClass.cardinal);
}

function formatChar(byte: number): string {
    switch (byte) {
        case 0x00:
            return "'\\0'";
        case 0x07:
            return "'\\a'";
        case 0x08:
            return "'\\b'";
        case 0x09:
            return "'\\t'";
        case 0x0A:
            return "'\\n'";
        case 0x0B:
            return "'\\v'";
        case 0x0C:
            return "'\\f'";
        case 0x0D:
            return "'\\r'";
        case 0x27:
            return "'\\\''";
        case 0x5C:
            return "'\\\\'";
        default:
            if (byte >= 0x20 && byte <= 0x7E) {
                return `'${String.fromCharCode(byte)}'`;
            }
            return `'\\x${byte.toString(16).padStart(2, '0')}'`;
    }
}

export class PrimitiveVariable extends AbstractVariable {
    logger: Logger;

    constructor(type: TCFSymbolContextData, symbolDetails: TCFSymbolContextData | undefined, nameProvider: NameProvider, valueProvider: RawValueProvider, log: Logger) {
        super(type, symbolDetails, nameProvider, valueProvider);
        this.logger = log;
    }

    async getChildren(): Promise<ClientVariable[]> {
        return [];
    }

    async displayValue(): Promise<string | undefined> {
        const variableRawValue = await this.valueProvider.value();
        if (!variableRawValue) {
            return undefined;
        }
        let bigEndian = this.valueProvider.isBigEndian();

        switch (this.type.TypeClass) {
            case TCFTypeClass.integer: // SIGNED integer
                switch (variableRawValue.length) {
                    case 8: //64bit
                        const variable64Value = bigEndian ? variableRawValue.readBigInt64BE() : variableRawValue.readBigInt64LE();

                        return String(variable64Value);
                    case 4: //32bit
                        const variable32Value = bigEndian ? variableRawValue.readInt32BE() : variableRawValue.readInt32LE();

                        return String(variable32Value);
                    case 2: //16 bit
                        const variable16Value = bigEndian ? variableRawValue.readInt16BE() : variableRawValue.readInt16LE();

                        return String(variable16Value);
                    case 1: //8 bit
                        const variable8Value = variableRawValue.readInt8();

                        return isCharLikeType(this.type) ? formatChar(variableRawValue.readUInt8()) : String(variable8Value);
                    default:
                        this.logger.log(`Could not parse integer variable value for variable ${this.name()}. Expected 2, 4 or 8 bytes, found ${variableRawValue.length} bytes`);
                        return undefined;
                }
            case TCFTypeClass.cardinal: // UNSIGNED integer
                switch (variableRawValue.length) {
                    case 8: //64bit
                        const variable64Value = bigEndian ? variableRawValue.readBigUInt64BE() : variableRawValue.readBigUInt64LE();

                        return String(variable64Value);
                    case 4: //32bit
                        const variable32Value = bigEndian ? variableRawValue.readUInt32BE() : variableRawValue.readUInt32LE();

                        return String(variable32Value);
                    case 2: //16 bit
                        const variable16Value = bigEndian ? variableRawValue.readUInt16BE() : variableRawValue.readUInt16LE();

                        return String(variable16Value);
                    case 1: //8 bit
                        const variable8Value = variableRawValue.readUInt8();

                        return isCharLikeType(this.type) ? formatChar(variable8Value) : String(variable8Value);
                    default:
                        this.logger.log(`Could not parse unsigned integer variable value for variable ${this.name()}. Expected 2, 4 or 8 bytes, found ${variableRawValue.length} bytes`);
                        return undefined;
                }
            case TCFTypeClass.real:
                switch (variableRawValue.length) {
                    case 8: //64 bit
                        const doubleValue = bigEndian ? variableRawValue.readDoubleBE() : variableRawValue.readDoubleLE();

                        return String(doubleValue);
                    case 4: //32bit
                        const floatValue = bigEndian ? variableRawValue.readFloatBE() : variableRawValue.readFloatLE();

                        return String(floatValue);
                    default:
                        this.logger.log(`Could not parse float/double variable value for variable ${this.name()}. Expected 4 or 8 bytes, found ${variableRawValue.length} bytes`);
                        return undefined;
                }
            default:
                this.logger.log(`Could not parse unknown primitive variable value for variable ${this.name()}, type class ${this.type.TypeClass}`);
                return undefined;
        }

    }
}

