import { afterEach, describe, expect, it } from "vitest";
import { MemoriaClient } from "../client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

describe("profile response validation", () => {
  it.each([null, {}, [], "unexpected", 42, { error: "private upstream detail" },
    { profile: false }, { profile: 0 }, { profile: [] }, { profile: {} }])(
    "rejects malformed profile response %j", async (body) => {
      const mock = mockFetch();
      mock.respondWith(200, body);
      const client = new MemoriaClient(buildApiConfig());
      try {
        await expect(client.profile("u")).rejects.toThrow("Invalid Memoria API profile response");
        expect(mock.calls).toHaveLength(1);
      } finally { client.close(); }
    },
  );

  it.each([null, "", "Likes TypeScript\nPrefers concise answers"])(
    "preserves valid profile %j", async (profile) => {
      const mock = mockFetch();
      mock.respondWith(200, { profile });
      const client = new MemoriaClient(buildApiConfig());
      try {
        expect(await client.profile("u")).toEqual({ user_id: "u", profile: profile || null });
      } finally { client.close(); }
    },
  );
});
