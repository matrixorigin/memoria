import { afterEach, describe, expect, it } from "vitest";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

describe("snapshot creation receipts", () => {
  it.each([null, {}, [], "unexpected", { error: "private upstream detail" },
    { name: null }, { name: "" }, { name: "  " }, { name: 42 }, { name: {} }])(
    "rejects malformed receipt %j without replay", async (body) => {
      const mock = mockFetch();
      mock.respondWith(201, body);
      const transport = new MemoriaHttpTransport(buildApiConfig(), "u");
      await expect(transport.callTool("memory_snapshot", { name: "requested" }))
        .rejects.toThrow("Invalid Memoria API snapshot creation response: missing name");
      expect(mock.calls).toHaveLength(1);
    },
  );

  it.each([{ timestamp: "2026-11-11T00:00:00Z" }, { created_at: "2026-11-11T00:00:00Z" }, {}])(
    "preserves authoritative name and optional timestamps %j", async (fields) => {
      const mock = mockFetch();
      mock.respondWith(201, { name: "actual", ...fields });
      const transport = new MemoriaHttpTransport(buildApiConfig(), "u");
      const result = await transport.callTool("memory_snapshot", { name: "requested" }) as { content: Array<{ text: string }> };
      expect(result.content[0].text).toBe(`Snapshot 'actual' created at ${"timestamp" in fields ? fields.timestamp : "created_at" in fields ? fields.created_at : ""}`);
    },
  );
});
