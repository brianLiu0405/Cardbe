import { expect, it, vi } from "vite-plus/test";
import { FlowImportCoordinator } from "./import-coordinator";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

it("drains layout and tab writes before replacement, rejects queued writes and unlocks only after reload", async () => {
  const gate = new FlowImportCoordinator();
  const layout = deferred(),
    tab = deferred(),
    reload = deferred();
  const events: string[] = [];
  const unregister = gate.register(7, {
    pause: () => events.push("pause"),
    reload: () => {
      events.push("reload");
      return reload.promise;
    },
    resume: () => events.push("resume"),
  });
  const layout_write = gate.write(7, () => layout.promise);
  const tab_write = gate.write(7, () => tab.promise);
  const imported = gate.replace(7, async () => {
    events.push("import");
    return "snapshot";
  });
  expect(events).toEqual(["pause"]);
  const stale = vi.fn(async () => {});
  await expect(gate.write(7, stale)).rejects.toEqual({
    code: "STALE_BOARD_REQUEST",
  });
  expect(stale).not.toHaveBeenCalled();
  layout.resolve();
  await layout_write;
  expect(events).toEqual(["pause"]);
  tab.resolve();
  await tab_write;
  await vi.waitFor(() => expect(events).toEqual(["pause", "import", "reload"]));
  expect(gate.is_blocked(7)).toBe(true);
  await expect(gate.write(7, stale)).rejects.toEqual({
    code: "STALE_BOARD_REQUEST",
  });
  // Another board remains usable during this board's import.
  await expect(gate.write(8, async () => "other board")).resolves.toBe(
    "other board",
  );
  reload.resolve();
  await expect(imported).resolves.toBe("snapshot");
  expect(events).toEqual(["pause", "import", "reload", "resume"]);
  expect(gate.is_blocked(7)).toBe(false);
  unregister();
});

it("tracks unmounted view writes and reloads views mounted during replacement", async () => {
  const gate = new FlowImportCoordinator();
  const write = deferred(),
    importing = deferred();
  const unregister = gate.register(7, {
    pause() {},
    async reload() {},
    resume() {},
  });
  const running = gate.write(7, () => write.promise);
  unregister();
  const action = vi.fn(() => importing.promise);
  const result = gate.replace(7, action);
  await Promise.resolve();
  expect(action).not.toHaveBeenCalled();
  const view = {
    pause: vi.fn(),
    reload: vi.fn(async () => {}),
    resume: vi.fn(),
  };
  const remove = gate.register(7, view);
  expect(view.pause).toHaveBeenCalledOnce();
  write.resolve();
  await running;
  await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
  importing.resolve();
  await result;
  expect(view.reload).toHaveBeenCalledOnce();
  expect(view.resume).toHaveBeenCalledOnce();
  remove();
});

it("reloads persisted state and releases the gate when import fails", async () => {
  const gate = new FlowImportCoordinator();
  const events: string[] = [];
  gate.register(7, {
    pause: () => events.push("pause"),
    reload: async () => {
      expect(gate.is_blocked(7)).toBe(true);
      events.push("reload old data");
    },
    resume: () => events.push("resume"),
  });
  await expect(
    gate.replace(7, async () => {
      throw new Error("invalid backup");
    }),
  ).rejects.toThrow("invalid backup");
  expect(events).toEqual(["pause", "reload old data", "resume"]);
  await expect(gate.write(7, async () => true)).resolves.toBe(true);
});
