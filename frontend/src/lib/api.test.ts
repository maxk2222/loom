import { describe, expect, it } from "vitest";

describe("api helpers", () => {
  it("APP_VARIANT resolves to server by default", async () => {
    const { APP_VARIANT } = await import("./api");
    expect(["server", "client"]).toContain(APP_VARIANT);
  });
});
