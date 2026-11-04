import { afterEach, describe, expect, it } from "vitest";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

describe.each([{}, { limit: 20, offset: 0 }])("snapshot response validation %j", (args) => {
  it.each([null, {}, { error: "private upstream detail" }, { snapshots: null },
    { items: {} }, 42, "invalid", { result: {} }])("rejects malformed response %j", async (body) => {
    const mock = mockFetch();
    mock.respondWith(200, body);
    const transport = new MemoriaHttpTransport(buildApiConfig(), "user");
    await expect(transport.callTool("memory_snapshots", args))
      .rejects.toThrow("Memoria snapshot listing returned a malformed page");
    expect(mock.calls).toHaveLength(1);
  });

  it.each([[], { snapshots: [] }, { items: [] }, { result: "Snapshots (0):" }])(
    "preserves valid empty response %j", async (body) => {
      const mock = mockFetch();
      mock.respondWith(200, body);
      const transport = new MemoriaHttpTransport(buildApiConfig(), "user");
      const result = await transport.callTool("memory_snapshots", args) as { content: Array<{ text: string }> };
      expect(result.content[0].text).toBe("Snapshots (0):");
    },
  );
});
