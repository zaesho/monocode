import { describe, expect, it, vi } from "vitest";
const sent = vi.hoisted(() => [] as string[]);
vi.mock("./child", () => ({
  writeChild: async (_id: string, line: string) => {
    sent.push(line);
  },
}));
import { AcpClient } from "./acp";

describe("ACP request identifiers", () => {
  it("preserves string and number types when internal approval ids collide", async () => {
    const requests: number[] = [];
    const client = new AcpClient("ids", {
      onRequest: (id) => {
        requests.push(id);
      },
    });
    const incoming = ["permission-abc", -1, "12", 12];
    for (const id of incoming)
      client.pushLine(
        JSON.stringify({
          jsonrpc: "2.0",
          id,
          method: "session/request_permission",
          params: {},
        }),
      );
    await Promise.resolve();
    expect(new Set(requests).size).toBe(incoming.length);
    for (const id of requests)
      await client.respond(id, { outcome: { outcome: "cancelled" } });
    expect(sent.map((line) => JSON.parse(line).id)).toEqual(incoming);
    client.close();
  });
});
