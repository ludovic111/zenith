/** Small JSON helpers shared by the validator, the normalizer and the differ. */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface JsonDifference {
  readonly path: string;
  readonly left: unknown;
  readonly right: unknown;
}

const isPlainObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const keyPath = (path: string, key: string | number) =>
  typeof key === "number"
    ? `${path}[${key}]`
    : /^[A-Za-z_$][\w$]*$/.test(key)
      ? `${path}.${key}`
      : `${path}[${JSON.stringify(key)}]`;

/**
 * Structural differences between two JSON values. Key order is ignored; a key that is absent on
 * one side is reported with `undefined` for that side, so "absent" and `null` stay distinct.
 */
export const jsonDiff = (
  left: unknown,
  right: unknown,
  path = "$",
  out: Array<JsonDifference> = [],
  limit = 200,
): Array<JsonDifference> => {
  if (out.length >= limit) return out;
  if (Object.is(left, right)) return out;
  if (Array.isArray(left) && Array.isArray(right)) {
    const n = Math.max(left.length, right.length);
    for (let i = 0; i < n; i++) {
      if (i >= left.length || i >= right.length) {
        out.push({ path: keyPath(path, i), left: left[i], right: right[i] });
        if (out.length >= limit) return out;
        continue;
      }
      jsonDiff(left[i], right[i], keyPath(path, i), out, limit);
    }
    return out;
  }
  if (isPlainObject(left) && isPlainObject(right)) {
    const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
    for (const key of keys) {
      const inLeft = Object.hasOwn(left, key);
      const inRight = Object.hasOwn(right, key);
      if (!inLeft || !inRight) {
        out.push({ path: keyPath(path, key), left: left[key], right: right[key] });
        if (out.length >= limit) return out;
        continue;
      }
      jsonDiff(left[key], right[key], keyPath(path, key), out, limit);
    }
    return out;
  }
  out.push({ path, left, right });
  return out;
};

/** A one-line preview of a value for reports. */
export const preview = (value: unknown, max = 160): string => {
  let text: string;
  try {
    text = value === undefined ? "<absent>" : JSON.stringify(value);
  } catch {
    text = String(value);
  }
  return text.length > max ? `${text.slice(0, max)}…(${text.length} chars)` : text;
};

/** Normalizes a decoded-then-re-encoded value so it can be compared with raw JSON. */
export const toPlainJson = (value: unknown): unknown =>
  value === undefined ? undefined : JSON.parse(JSON.stringify(value));
