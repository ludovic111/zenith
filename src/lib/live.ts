/**
 * Values that follow the configuration. The config can change while zenith runs (the
 * settings and the welcome write it), so what used to be read once at startup is read
 * again on every access, through a stand-in that behaves like the real array or object.
 */

export function liveArray<T>(get: () => T[]): T[] {
  return new Proxy([] as T[], {
    get: (_, k) => {
      const a = get();
      const v = Reflect.get(a, k, a);
      return typeof v === "function" ? v.bind(a) : v;
    },
    has: (_, k) => Reflect.has(get(), k),
    ownKeys: () => Reflect.ownKeys(get()),
    getOwnPropertyDescriptor: (_, k) => {
      const d = Reflect.getOwnPropertyDescriptor(get(), k);
      // `length` is not configurable on arrays; the stand-in's own says so already.
      return d && k !== "length" ? { ...d, configurable: true } : d;
    },
  });
}

export function liveObject<T extends object>(get: () => T): T {
  return new Proxy({} as T, {
    get: (_, k) => Reflect.get(get(), k),
    has: (_, k) => Reflect.has(get(), k),
    ownKeys: () => Reflect.ownKeys(get()),
    getOwnPropertyDescriptor: (_, k) => {
      const d = Reflect.getOwnPropertyDescriptor(get(), k);
      return d ? { ...d, configurable: true } : d;
    },
  });
}
