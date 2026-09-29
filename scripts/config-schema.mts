// Writes zenith.schema.json from the zod schema, for autocompletion in editors.
// Run: npm run config:schema
import { writeFileSync } from "node:fs";
import { z } from "zod";
import { ConfigSchema } from "../src/lib/config-schema.ts";

writeFileSync(new URL("../zenith.schema.json", import.meta.url), JSON.stringify(z.toJSONSchema(ConfigSchema, { io: "input" }), null, 2) + "\n");
console.log("zenith.schema.json written");
