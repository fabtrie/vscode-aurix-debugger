/**
 * C++ variable names from ELF symbol names (Itanium C++ ABI mangling, as GCC and Clang write it).
 *
 * Only what names a variable in an expression the debug server evaluates: namespaces and classes
 * (`_ZN3app5Motor5countE` -> `app::Motor::count`). Templates, substitutions and ABI tags are not
 * decoded; no expression can name such a symbol anyway.
 */

/**
 * The expression name of a variable symbol: `raw` itself when it is not mangled, `undefined` for
 * symbols that are no variable of the program (vtables, typeinfo, guard variables) or that no
 * expression can name (static variables inside functions, template instances).
 */
export function cppVariableName(raw: string): string | undefined {
	if (!raw.startsWith('_Z')) {
		return raw;
	}
	// _ZT*: vtable, VTT, typeinfo and its name, thunks, TLS wrappers; _ZGV: guard variables;
	// _ZZ: a static variable of a function; _ZGR: a temporary bound to a static reference.
	if (/^_Z(T|GV|GR|Z)/.test(raw)) {
		return undefined;
	}
	// `L`: internal linkage (a `static` variable at namespace scope).
	const parts = raw.startsWith('_ZN') ? nestedName(raw, 3) : sourceName(raw, raw.startsWith('_ZL') ? 3 : 2);
	return parts;
}

/** `<length><identifier>` at `pos` as the whole rest of `s`: `[identifier]`. */
function sourceName(s: string, pos: number): string | undefined {
	const m = /^(\d+)/.exec(s.slice(pos));
	if (!m) {
		return undefined;
	}
	const len = Number(m[1]);
	const start = pos + m[1].length;
	return start + len === s.length ? s.slice(start) : undefined;
}

/** `N [qualifiers] <name>+ E` from `pos` (after the `N`) to the end of `s`, joined with `::`. */
function nestedName(s: string, pos: number): string | undefined {
	while (pos < s.length && 'rVK'.includes(s[pos])) {
		pos++;
	}
	const parts: string[] = [];
	if (s.startsWith('St', pos)) {
		parts.push('std');
		pos += 2;
	}
	while (pos < s.length && s[pos] !== 'E') {
		if (s[pos] === 'L') {
			pos++;
		}
		const m = /^(\d+)/.exec(s.slice(pos));
		if (!m) {
			// A template, substitution, ABI tag, operator or constructor: not decoded.
			return undefined;
		}
		const len = Number(m[1]);
		const start = pos + m[1].length;
		if (len === 0 || start + len > s.length) {
			return undefined;
		}
		const id = s.slice(start, start + len);
		if (id.startsWith('_GLOBAL__N')) {
			// An anonymous namespace has no name in an expression; the debug server finds what is
			// declared in it by the rest of the name.
			parts.length = 0;
		} else {
			parts.push(id);
		}
		pos = start + len;
	}
	return pos === s.length - 1 && parts.length > 0 ? parts.join('::') : undefined;
}
