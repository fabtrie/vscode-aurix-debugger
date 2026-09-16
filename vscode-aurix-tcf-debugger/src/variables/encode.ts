/*
Encoding of values typed in the UI (Variables, Watch, Live Watch) into target bytes.
*/
import { TCFSymbolContextData, TCFTypeClass } from "../tcf-all";

/** Enumerators of an enumeration type: name and value bytes. */
export type EnumeratorLookup = () => Promise<Array<{ name: string; value: Buffer }>>;

const CHAR_ESCAPES: Record<string, number> = { n: 10, t: 9, r: 13, '0': 0, a: 7, b: 8, f: 12, v: 11, '\\': 92, "'": 39, '"': 34 };

/**
 * Parses an integer: decimal, 0x hex, 0b binary, 'c' character literal or true/false.
 */
export function parseInteger(text: string): bigint {
    const t = text.trim().replace(/_/g, '');
    if (/^(true|false)$/i.test(t)) {
        return t.toLowerCase() === 'true' ? 1n : 0n;
    }
    const ch = /^'(\\.|[^\\'])'$/.exec(t);
    if (ch) {
        const c = ch[1];
        if (c.length === 1) {
            return BigInt(c.charCodeAt(0));
        }
        const code = CHAR_ESCAPES[c[1]];
        if (code === undefined) {
            throw new Error(`Invalid character literal ${t}`);
        }
        return BigInt(code);
    }
    const m = /^([+-]?)(0x[0-9a-f]+|0b[01]+|[0-9]+)[ul]*$/i.exec(t);
    if (!m) {
        throw new Error(`Invalid integer value '${text}' (use decimal, 0x hex, 0b binary or a character)`);
    }
    const v = BigInt(m[2].toLowerCase());
    return m[1] === '-' ? -v : v;
}

/**
 * Encodes `text` as a value of `type` with `size` bytes. Integers may be given in the signed or
 * unsigned range of the size; enumerations also accept enumerator names.
 */
export async function encodeVariableValue(
    text: string,
    type: TCFSymbolContextData,
    size: number,
    enumerators?: EnumeratorLookup,
    bigEndian = false,
): Promise<Buffer> {
    const t = text.trim();
    const buf = Buffer.alloc(size);
    const typeName = type.Name ?? 'this type';

    if (type.TypeClass === TCFTypeClass.real) {
        const n = Number(t);
        if (t === '' || Number.isNaN(n)) {
            throw new Error(`Invalid floating point value '${text}'`);
        }
        if (size === 4) {
            bigEndian ? buf.writeFloatBE(n) : buf.writeFloatLE(n);
        } else if (size === 8) {
            bigEndian ? buf.writeDoubleBE(n) : buf.writeDoubleLE(n);
        } else {
            throw new Error(`Unsupported floating point size ${size}`);
        }
        return buf;
    }

    if (type.TypeClass === TCFTypeClass.enumeration && enumerators && /^[A-Za-z_]\w*$/.test(t) && !/^(true|false)$/i.test(t)) {
        const e = (await enumerators()).find((x) => x.name === t);
        if (!e) {
            throw new Error(`'${t}' is not an enumerator of ${typeName}`);
        }
        e.value.copy(buf, 0, 0, Math.min(size, e.value.length));
        return buf;
    }

    if (
        type.TypeClass === TCFTypeClass.integer ||
        type.TypeClass === TCFTypeClass.cardinal ||
        type.TypeClass === TCFTypeClass.pointer ||
        type.TypeClass === TCFTypeClass.enumeration
    ) {
        const v = parseInteger(t);
        const bits = BigInt(size * 8);
        if (size === 0 || v < -(1n << (bits - 1n)) || v >= 1n << bits) {
            throw new Error(`Value '${text}' does not fit into ${size} byte(s)`);
        }
        let u = v < 0n ? (1n << bits) + v : v;
        for (let i = 0; i < size; i++) {
            buf[bigEndian ? size - 1 - i : i] = Number(u & 0xffn);
            u >>= 8n;
        }
        return buf;
    }

    throw new Error(`Values of type '${typeName}' cannot be set directly`);
}
