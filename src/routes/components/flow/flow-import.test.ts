import { readFileSync } from "node:fs";
import { parse } from "svelte/compiler";
import { transpile } from "typescript";
import { expect, it, vi } from "vite-plus/test";
import { FlowImportCoordinator } from "./import-coordinator";

const source = readFileSync(
  new URL("./flow_view.svelte", import.meta.url),
  "utf8",
);
const names = [
  "import_blocked",
  "write_flow",
  "pause_import",
  "load",
  "refresh_notes",
  "save_layout",
  "finish_saving",
  "wait_idle",
  "schedule_viewport_save",
  "flush_viewport_save",
];
const functions = parse(source, { modern: true })
  .instance!.content.body.filter(
    (node) =>
      node.type === "FunctionDeclaration" && names.includes(node.id!.name),
  )
  .map((node) => {
    const { start, end } = node as typeof node & { start: number; end: number };
    return source.slice(start, end);
  })
  .join("\n");

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function flow(task: number) {
  return {
    tab_id: "default",
    tabs: [{ id: "default", name: "" }],
    relationships: [],
    layout: { nodes: [{ task_id: task, x: task, y: 0 }], viewport: null },
  };
}
function view(gate: FlowImportCoordinator, invoke: ReturnType<typeof vi.fn>) {
  return new Function(
    "flow_imports",
    "invoke",
    "initial",
    transpile(`
    let import_paused = false, disposed = false, loaded = true, saving = false, read_only = false;
    let load_request = 0, notes_request = 0, archive_request = 0;
    let board_id = 7, tab_id = "default", requested_tab = "";
    let data = initial, tabs = initial.tabs, nodes = [], edges = [], notes = [], canvas_archives = [];
    let viewport = {x: 10, y: 0, zoom: 1}, viewport_timer, viewport_save_delay = 800;
    let pending_layout = null, idle_waiters = [], selected_edges = [], editing_annotation_id = null;
    let canvas_ready = true, add_open = false, note_picker_open = false, renaming_tab_id = null, deleting_tab = null;
    let load_error = null, notes_loading = false, notes_error = null;
    const remember_tab = () => {}, toast = { error() {} }, translateCommandError = () => "error";
    const save_positions = () => save_layout(data.layout);
    ${functions}
    return {
      pause: pause_import,
      reload: async () => { await Promise.all([load("", true), refresh_notes(true)]); },
      resume: () => { import_paused = false; },
      load, refresh_notes, save_layout, schedule_viewport_save,
      destroy: () => { flush_viewport_save(); disposed = true; },
      state: () => ({ data, notes, pending_layout, saving, loaded }),
    };
  `),
  )(gate, invoke, flow(1));
}

it("cancels queued layouts and viewport flush before import, then saves from the imported layout", async () => {
  const gate = new FlowImportCoordinator();
  const pending = deferred<void>();
  const invoke = vi.fn((command: string) => {
    if (command === "save_flow_layout") {
      return pending.promise;
    }
    return Promise.resolve(
      command === "get_flow" ? flow(99) : [{ id: 99, title: "Imported note" }],
    );
  });
  const canvas = view(gate, invoke);
  const unregister = gate.register(7, canvas);
  const first = canvas.save_layout(flow(1).layout);
  await canvas.save_layout(flow(2).layout);
  canvas.schedule_viewport_save();
  const imported = vi.fn(async () => {});
  const replacing = gate.replace(7, imported);
  expect(canvas.state().pending_layout).toBeNull();
  expect(imported).not.toHaveBeenCalled();
  pending.resolve();
  await first;
  await replacing;
  expect(
    invoke.mock.calls.filter(([command]) => command === "save_flow_layout"),
  ).toHaveLength(1);
  expect(canvas.state().data).toEqual(flow(99));
  expect(canvas.state().notes).toEqual([{ id: 99, title: "Imported note" }]);
  await canvas.save_layout(canvas.state().data.layout);
  expect(invoke).toHaveBeenLastCalledWith(
    "save_flow_layout",
    expect.objectContaining({ layout: flow(99).layout }),
  );
  canvas.destroy();
  unregister();
});

it("discards pre-import Flow and note responses and never flushes a disposed canvas across the gate", async () => {
  const gate = new FlowImportCoordinator();
  const old_flow = deferred<unknown>(),
    old_notes = deferred<unknown>();
  let flow_reads = 0,
    note_reads = 0;
  const invoke = vi.fn((command: string) => {
    if (command === "get_flow") {
      return ++flow_reads === 1 ? old_flow.promise : Promise.resolve(flow(99));
    }
    if (command === "get_notes") {
      return ++note_reads === 1
        ? old_notes.promise
        : Promise.resolve([{ id: 99 }]);
    }
    return Promise.resolve();
  });
  const canvas = view(gate, invoke);
  const unregister = gate.register(7, canvas);
  const loading = canvas.load(),
    notes_loading = canvas.refresh_notes();
  await gate.replace(7, async () => {});
  old_flow.resolve(flow(1));
  old_notes.resolve([{ id: 1 }]);
  await Promise.all([loading, notes_loading]);
  expect(canvas.state().data).toEqual(flow(99));
  expect(canvas.state().notes).toEqual([{ id: 99 }]);
  canvas.schedule_viewport_save();
  const import_pending = deferred<void>();
  const replacing = gate.replace(7, () => import_pending.promise);
  canvas.destroy();
  unregister();
  import_pending.resolve();
  await replacing;
  expect(
    invoke.mock.calls.some(([command]) => command === "save_flow_layout"),
  ).toBe(false);
});
