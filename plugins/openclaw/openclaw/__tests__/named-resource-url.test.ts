import { afterEach, expect, it, vi } from "vitest";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig } from "./helpers.js";

afterEach(() => vi.unstubAllGlobals());

const routes = [
  ["memory_checkout", "name", "branches", "/checkout"],
  ["memory_branch_delete", "name", "branches", ""],
  ["memory_merge", "source", "branches", "/merge"],
  ["memory_diff", "source", "branches", "/diff"],
  ["memory_rollback", "name", "snapshots", "/rollback"],
] as const;

for (const [tool, selector, collection, suffix] of routes) {
  it.each([".", ".."])(`${tool} rejects navigation selector %s before fetch`, async (name) => {
    const fetch = vi.fn().mockResolvedValue(new Response("{}"));
    vi.stubGlobal("fetch", fetch);
    const transport = new MemoriaHttpTransport(buildApiConfig(), "test-user");
    await expect(transport.callTool(tool, { [selector]: name })).rejects.toThrow(/dot-only/);
    expect(fetch).not.toHaveBeenCalled();
  });

  it.each(["nested/name", "literal%2E", "版本😀", "name?query#fragment"])(
    `${tool} preserves literal selector %s`, async (name) => {
      const requests: Request[] = [];
      vi.stubGlobal("fetch", async (input: string, init: RequestInit) => {
        requests.push(new Request(input, init));
        return new Response("{}");
      });
      const transport = new MemoriaHttpTransport(buildApiConfig(), "test-user");
      await transport.callTool(tool, { [selector]: name });
      const url = new URL(requests[0].url);
      expect(url.pathname).toBe(`/v1/${collection}/${encodeURIComponent(name)}${suffix}`);
      expect(url.hash).toBe("");
      expect(url.search).toBe(tool === "memory_diff" ? "?limit=50" : "");
    },
  );
}
