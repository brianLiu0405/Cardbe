import { readFileSync } from "node:fs";
import { parse } from "svelte/compiler";
import { transpile } from "typescript";
import { expect, it, vi } from "vite-plus/test";
import {
  dragged_tab_ids,
  moved_layout,
  moved_tab_ids,
  tab_after_delete,
  type FlowLayout,
} from "./flow";

const source = readFileSync(
  new URL("./flow_view.svelte", import.meta.url),
  "utf8",
);
function extract(names: string[]) {
  return parse(source, { modern: true })
    .instance!.content.body.filter(
      (node) =>
        node.type === "FunctionDeclaration" && names.includes(node.id!.name),
    )
    .map((node) => {
      const { start, end } = node as typeof node & {
        start: number;
        end: number;
      };
      return source.slice(start, end);
    })
    .join("\n");
}
const tabs = [
  { id: "a", name: "總覽" },
  { id: "b", name: "" },
  { id: "c", name: "🐈" },
];

it("reorders tabs only within bounds", () => {
  expect(moved_tab_ids(tabs, "a", 1)).toEqual(["b", "a", "c"]);
  expect(moved_tab_ids(tabs, "c", -2)).toEqual(["c", "a", "b"]);
  expect(moved_tab_ids(tabs, "a", -1)).toBeNull();
  expect(moved_tab_ids(tabs, "c", 1)).toBeNull();
  expect(moved_tab_ids(tabs, "missing", 1)).toBeNull();
  expect(moved_tab_ids(tabs, "b", 0)).toBeNull();
});

it("reorders tabs while dragging over another tab", () => {
  const ids = tabs.map((tab) => tab.id);
  expect(dragged_tab_ids(ids, "a", "c")).toEqual(["b", "c", "a"]);
  expect(dragged_tab_ids(ids, "c", "a")).toEqual(["c", "a", "b"]);
  expect(dragged_tab_ids(ids, "b", "b")).toBeNull();
  expect(dragged_tab_ids(ids, "missing", "a")).toBeNull();
});

it("selects the right neighbor after deleting a tab and keeps the last tab", () => {
  expect(tab_after_delete(tabs, "a")).toBe("b");
  expect(tab_after_delete(tabs, "b")).toBe("c");
  expect(tab_after_delete(tabs, "c")).toBe("b");
  expect(tab_after_delete([tabs[0]], "a")).toBeNull();
});

it("waits for queued saves before loading another tab", async () => {
  const load = vi.fn(async (_id: string) => {});
  const save = vi.fn();
  const code = transpile(`
    let saving = true, disposed = false, loaded = true, tab_id = "a", requested_tab = "";
    let pending_layout = { nodes: [] }, idle_waiters = [];
    const flush_viewport_save = () => {};
    const import_blocked = () => false;
    const data = { layout: { connections: {} } };
    function save_layout(layout) {
      save(layout, tab_id);
      saving = true;
      pending_layout = null;
    }
    ${extract(["finish_saving", "wait_idle", "switch_tab"])}
    return { finish_saving, switch_tab, state: () => ({ saving, tab_id }) };
  `);
  const view = new Function("load", "save", code)(load, save);
  const switched = view.switch_tab("b");
  await Promise.resolve();
  expect(load).not.toHaveBeenCalled();
  // The first save completes, which flushes the queued layout for the old tab.
  view.finish_saving();
  expect(save).toHaveBeenCalledWith(expect.anything(), "a");
  await Promise.resolve();
  expect(load).not.toHaveBeenCalled();
  view.finish_saving();
  await switched;
  expect(load).toHaveBeenCalledWith("b");
});

it("removes canvas items without discarding unsaved positions", () => {
  const layout: FlowLayout = {
    nodes: [
      { task_id: 1, x: 0, y: 0 },
      { task_id: 2, x: 0, y: 0 },
    ],
    annotations: [
      { id: "text-1", kind: "text", text: "說明", x: 0, y: 0, width: 280 },
      { id: "text-2", kind: "text", text: "", x: 0, y: 0, width: 280 },
    ],
    viewport: null,
  };
  const nodes = [
    { id: "task_1", position: { x: 1, y: 2 }, data: {} },
    { id: "task_2", position: { x: 3, y: 4 }, data: {} },
    { id: "annotation_text-1", position: { x: 5, y: 6 }, width: 420, data: {} },
    { id: "annotation_text-2", position: { x: 7, y: 8 }, data: {} },
  ];
  const without_nodes = new Function(
    "moved_layout",
    "data",
    "nodes",
    "viewport",
    "pending_layout",
    transpile(`${extract(["without_nodes"])}\nreturn without_nodes;`),
  )(moved_layout, { layout }, nodes, { x: 0, y: 0, zoom: 1 }, null);
  const result: FlowLayout = without_nodes(
    new Set(["task_2", "annotation_text-2"]),
  );
  expect(result.nodes).toEqual([{ task_id: 1, x: 1, y: 2 }]);
  expect(result.annotations).toEqual([
    { id: "text-1", kind: "text", text: "說明", x: 5, y: 6, width: 280 },
  ]);
});

it("saves the viewport once after panning settles", () => {
  vi.useFakeTimers();
  try {
    const save = vi.fn();
    const code = transpile(`
      let viewport_timer, viewport = { x: 0, y: 0, zoom: 1 };
      const viewport_save_delay = 800;
      const data = { layout: { viewport: { x: 0, y: 0, zoom: 1 } } };
      const save_positions = save;
      const import_blocked = () => false;
      ${extract(["schedule_viewport_save", "flush_viewport_save"])}
      return {
        schedule_viewport_save,
        flush_viewport_save,
        pan: (x) => (viewport = { ...viewport, x }),
      };
    `);
    const view = new Function("save", code)(save);
    // An unchanged viewport, such as a click without panning, is not saved.
    view.schedule_viewport_save();
    vi.advanceTimersByTime(800);
    expect(save).not.toHaveBeenCalled();
    view.pan(10);
    view.schedule_viewport_save();
    vi.advanceTimersByTime(400);
    view.pan(20);
    view.schedule_viewport_save();
    vi.advanceTimersByTime(799);
    expect(save).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(save).toHaveBeenCalledTimes(1);
    // Switching tabs flushes a pending viewport immediately and only once.
    view.pan(30);
    view.schedule_viewport_save();
    view.flush_viewport_save();
    expect(save).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(800);
    view.flush_viewport_save();
    expect(save).toHaveBeenCalledTimes(2);
  } finally {
    vi.useRealTimers();
  }
});
