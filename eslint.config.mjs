import { defineConfig, globalIgnores } from "eslint/config";
import nextVitals from "eslint-config-next/core-web-vitals";
import nextTs from "eslint-config-next/typescript";

const eslintConfig = defineConfig([
  ...nextVitals,
  ...nextTs,
  {
    // Server components render on every request: the current time is data there.
    files: ["src/app/**", "src/components/blocks/**", "src/components/plans/**", "src/components/identity/**", "perso/**"],
    rules: { "react-hooks/purity": "off" },
  },
  // Override default ignores of eslint-config-next.
  globalIgnores([
    // Default ignores of eslint-config-next:
    ".next/**",
    "out/**",
    "build/**",
    "next-env.d.ts",
    // Magic UI components, copied as is.
    "src/components/ui/**",
    // zenith code has its own toolchain (oxlint, tsgo).
    "code/**",
    "publish/**",
  ]),
]);

export default eslintConfig;
