import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, expect, it } from "vitest";

const script = fileURLToPath(new URL("../../scripts/connect_openclaw_memoria.mjs", import.meta.url));
let directory: string;
let config: string;

beforeEach(() => {
  directory = mkdtempSync(join(tmpdir(), "memoria-connect-"));
  config = join(directory, "openclaw.json");
});
afterEach(() => rmSync(directory, { recursive: true, force: true }));

function connect(...extraArgs: string[]) {
  return spawnSync(process.execPath, [
    script, "--config-file", config, "--api-url", "https://example.invalid", "--api-key", "fixture", ...extraArgs,
  ], { encoding: "utf8" });
}

it.each([
  ["root", ["original"]],
  ["plugins", { plugins: null }],
  ["plugins.entries", { plugins: { entries: ["original"] } }],
  ["plugins.slots", { plugins: { slots: "original" } }],
  ["plugins.entries.thememoria", { plugins: { entries: { thememoria: false } } }],
  ["plugins.entries.thememoria.config", { plugins: { entries: { thememoria: { config: 3 } } } }],
  ["plugins.allow", { plugins: { allow: "original" } }],
  ["plugins.allow", { plugins: { allow: ["original", null] } }],
])("preserves an incompatible %s field", (field, value) => {
  const original = JSON.stringify(value, null, 4) + "\n";
  writeFileSync(config, original);
  const result = connect();
  expect(result.status).toBe(1);
  expect(result.stderr).toContain(field);
  expect(readFileSync(config, "utf8")).toBe(original);
});

it("adds the plugin while preserving existing settings", () => {
  writeFileSync(config, JSON.stringify({ plugins: { allow: ["other"], entries: { other: {} } } }));
  expect(connect().status).toBe(0);
  expect(JSON.parse(readFileSync(config, "utf8")).plugins).toMatchObject({
    allow: ["other", "thememoria"], entries: { other: {}, thememoria: { enabled: true } },
  });
});

it.each(["constructor", "__proto__"])("creates the requested own entry for %s", (id) => {
  writeFileSync(config, JSON.stringify({ plugins: { entries: { other: { enabled: false } } } }));
  expect(connect("--plugin-id", id).status).toBe(0);
  const { entries } = JSON.parse(readFileSync(config, "utf8")).plugins;
  expect(Object.hasOwn(entries, id)).toBe(true);
  expect(entries[id].enabled).toBe(true);
  expect(entries.other).toEqual({ enabled: false });
});
