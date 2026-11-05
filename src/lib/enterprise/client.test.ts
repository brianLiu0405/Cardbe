import { describe, expect, it, vi } from "vite-plus/test";
import {
  EnterpriseClient,
  EnterpriseError,
  asEnterpriseError,
  prepareCommand,
} from "./client.js";
import type { Command } from "./types.js";

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("enterprise prototype transport", () => {
  it("keeps credentials out of URLs and sends no caller-supplied actor", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(json({ workspaces: [] }));
    const client = new EnterpriseClient("  private-token  ", vi.fn(), fetcher);
    await client.workspaces();
    expect(fetcher).toHaveBeenCalledWith(
      "/enterprise-api/v1/workspaces",
      expect.objectContaining({
        method: "GET",
        headers: {
          Accept: "application/json",
          Authorization: "Bearer private-token",
        },
        credentials: "omit",
        cache: "no-store",
        redirect: "error",
      }),
    );
    expect(fetcher.mock.calls[0][1]?.body).toBeUndefined();
    client.dispose();
  });

  it("does not attach the token to a public health request", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(json({ status: "ok" }));
    const client = new EnterpriseClient("private-token", vi.fn(), fetcher);
    await client.health();
    expect(fetcher.mock.calls[0][1]?.headers).toEqual({
      Accept: "application/json",
    });
    client.dispose();
  });

  it("retries the original ID and payload even after the source draft changes", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockRejectedValueOnce(
        new TypeError("unreachable host with private details"),
      )
      .mockResolvedValueOnce(
        json({
          operation_id: "stable-id",
          revision: 1,
          replayed: true,
          result: {},
        }),
      );
    const client = new EnterpriseClient("token", vi.fn(), fetcher);
    const command: Command = {
      type: "task.create",
      title: "Original",
      description: "Draft",
    };
    const pending = prepareCommand("board-a", command, "stable-id");
    command.title = "Edited after sending";
    await expect(client.submit(pending)).rejects.toMatchObject({
      code: "NETWORK_ERROR",
      outcomeUnknown: true,
    });
    await expect(client.submit(pending)).resolves.toMatchObject({
      replayed: true,
    });
    const bodies = fetcher.mock.calls.map((call) =>
      JSON.parse(String(call[1]?.body)),
    );
    expect(bodies[0]).toEqual(bodies[1]);
    expect(bodies[0]).toEqual({
      operation_id: "stable-id",
      command: { type: "task.create", title: "Original", description: "Draft" },
    });
    expect(Object.isFrozen(pending.request.command)).toBe(true);
    client.dispose();
  });

  it.each([401, 403])(
    "invalidates the session on HTTP %s even without a valid error body",
    async (status) => {
      const denied = vi.fn();
      const fetcher = vi
        .fn<typeof fetch>()
        .mockResolvedValue(new Response("private raw error", { status }));
      const client = new EnterpriseClient("secret", denied, fetcher);
      const code = status === 401 ? "UNAUTHENTICATED" : "PERMISSION_DENIED";
      await expect(client.me()).rejects.toMatchObject({ code });
      expect(denied).toHaveBeenCalledExactlyOnceWith(code);
      await expect(client.members()).rejects.toMatchObject({
        code: "SESSION_ENDED",
      });
      expect(fetcher).toHaveBeenCalledTimes(1);
      expect(fetcher.mock.calls[0][1]?.signal?.aborted).toBe(true);
    },
  );

  it("does not deliver an old in-flight response after revocation", async () => {
    let resolveFirst: ((value: Response) => void) | undefined;
    const fetcher = vi
      .fn<typeof fetch>()
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveFirst = resolve;
          }),
      )
      .mockResolvedValueOnce(json({ code: "PERMISSION_DENIED" }, 403));
    const denied = vi.fn();
    const client = new EnterpriseClient("secret", denied, fetcher);
    const inFlight = client.workspaces();
    await expect(client.members()).rejects.toMatchObject({
      code: "PERMISSION_DENIED",
    });
    resolveFirst?.(
      json({ workspaces: [{ id: "private", name: "Restricted" }] }),
    );
    await expect(inFlight).rejects.toMatchObject({ code: "SESSION_ENDED" });
    expect(denied).toHaveBeenCalledTimes(1);
  });

  it("preserves omitted versus explicit-null assignee semantics", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockImplementation(async () => json({ result: {} }));
    const client = new EnterpriseClient("token", vi.fn(), fetcher);
    await client.submit(
      prepareCommand(
        "board",
        {
          type: "task.update",
          task_id: "task",
          base_version: 3,
          title: "Keep owner",
        },
        "keep-id",
      ),
    );
    await client.submit(
      prepareCommand(
        "board",
        {
          type: "task.update",
          task_id: "task",
          base_version: 3,
          assignee_id: null,
        },
        "clear-id",
      ),
    );
    const keep = JSON.parse(String(fetcher.mock.calls[0][1]?.body));
    const clear = JSON.parse(String(fetcher.mock.calls[1][1]?.body));
    expect(keep.command).not.toHaveProperty("assignee_id");
    expect(clear.command.assignee_id).toBeNull();
    client.dispose();
  });

  it("exposes stable codes without backend detail or untrusted messages", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        json({ code: "CONFLICT", detail: "/private/database" }, 409),
      )
      .mockResolvedValueOnce(
        json({ code: "UNKNOWN", message: "private token" }, 500),
      );
    const client = new EnterpriseClient("token", vi.fn(), fetcher);
    await expect(client.snapshot("board")).rejects.toEqual(
      new EnterpriseError("CONFLICT"),
    );
    await expect(client.snapshot("board")).rejects.toEqual(
      new EnterpriseError("INTERNAL_ERROR", true),
    );
    expect(asEnterpriseError(new Error("private backend detail")).message).toBe(
      "INTERNAL_ERROR",
    );
    client.dispose();
  });

  it("encodes resource IDs instead of allowing path or query injection", async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(json({ boards: [] }));
    const client = new EnterpriseClient("token", vi.fn(), fetcher);
    await client.boards("workspace/one&actor_id=owner");
    expect(fetcher.mock.calls[0][0]).toBe(
      "/enterprise-api/v1/boards?workspace_id=workspace%2Fone%26actor_id%3Downer",
    );
    client.dispose();
  });
});
