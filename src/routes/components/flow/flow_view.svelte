<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import * as InputGroup from "#lib/components/ui/input-group/index.js";
  import { ScrollArea } from "#lib/components/ui/scroll-area/index.js";
  import SearchIcon from "@lucide/svelte/icons/search";
  import { Checkbox } from "#lib/components/ui/checkbox/index.js";
  import AnnotationNodeComponent from "./annotation_node.svelte";
  import { invoke } from "@tauri-apps/api/core";
  import {
    SvelteFlow,
    Background,
    Controls,
    ConnectionMode,
    SelectionMode,
    type Edge,
    type Viewport,
    type Connection,
    type OnBeforeDelete,
  } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import { Button, buttonVariants } from "#lib/components/ui/button/index.js";
  import * as DropdownMenu from "#lib/components/ui/dropdown-menu/index.js";
  import * as ContextMenu from "#lib/components/ui/context-menu/index.js";
  import * as Popover from "#lib/components/ui/popover/index.js";
  import PlusIcon from "@lucide/svelte/icons/plus";
  import ChevronDownIcon from "@lucide/svelte/icons/chevron-down";
  import CircleHelpIcon from "@lucide/svelte/icons/circle-help";
  import ListPlusIcon from "@lucide/svelte/icons/list-plus";
  import TypeIcon from "@lucide/svelte/icons/type";
  import FileTextIcon from "@lucide/svelte/icons/file-text";
  import UnlinkIcon from "@lucide/svelte/icons/unlink";
  import * as Dialog from "#lib/components/ui/dialog/index.js";
  import * as AlertDialog from "#lib/components/ui/alert-dialog/index.js";
  import FlowTabs from "./flow_tabs.svelte";
  import { flow_imports } from "./import-coordinator";
  import { Separator } from "#lib/components/ui/separator/index.js";
  import * as Field from "#lib/components/ui/field/index.js";
  import * as Empty from "#lib/components/ui/empty/index.js";
  import TaskSelectionPicker from "../task/task_selection_picker.svelte";
  import { toast } from "svelte-sonner";
  import {
    parseCommandError,
    translateCommandError,
  } from "#lib/command-errors.js";
  import { formatNumber } from "#lib/i18n/index.js";
  import * as m from "#lib/paraglide/messages.js";
  import type { Column } from "../../type/column.svelte";
  import {
    get_task_id,
    deserialize_task,
    type TaskSerialized,
    type Task,
  } from "../../type/task.svelte";
  import type { Archive } from "../../type/archive.svelte";
  import TaskNodeComponent from "./task_node.svelte";
  import TaskEdgeComponent from "./task_edge.svelte";
  import {
    build_nodes,
    build_annotations,
    type CanvasNode,
    type FlowAnnotation,
    type NotePreview,
    build_edges,
    moved_layout,
    scaled_layout,
    added_layout,
    connected_layout,
    available_task_columns,
    missing_archive_task_ids,
    moved_tab_ids,
    tab_after_delete,
    relationship_id,
    type RelationshipEdge,
    type FlowData,
    type FlowTab,
    type FlowLayout,
    type Relationship,
  } from "./flow";

  let {
    onOpenNote,
    note_revision = 0,
    board_id,
    columns,
    archives = [],
    onViewArchivedTask,
    onCopyTaskLink,
    onArchiveTask,
    onDeleteTask,
    search_task_ids = null,
    read_only: board_read_only = false,
    onViewTask,
    onEditTask,
  }: {
    onOpenNote: (id: number) => void;
    note_revision?: number;
    board_id: number;
    columns: Column[];
    archives?: Archive[];
    onViewArchivedTask: (task: Task) => void;
    onCopyTaskLink?: (task: Task) => void;
    onArchiveTask: (task: Task) => Promise<boolean>;
    onDeleteTask: (task: Task) => void;
    search_task_ids?: ReadonlySet<string> | null;
    read_only?: boolean;
    onViewTask: (task: Task) => void;
    onEditTask: (task: Task) => void;
  } = $props();
  let import_paused = $state(false);
  const read_only = $derived(board_read_only || import_paused);
  function import_blocked() {
    return import_paused || flow_imports.is_blocked(board_id);
  }
  function write_flow<T>(action: () => Promise<T>) {
    return flow_imports.write(board_id, action);
  }
  const nodeTypes = {
    task: TaskNodeComponent,
    annotation: AnnotationNodeComponent,
  };
  const edgeTypes = { task: TaskEdgeComponent };
  const tab_storage_key = `cardbe-flow-tab:${untrack(() => board_id)}`;
  function recalled_tab() {
    try {
      return window.localStorage.getItem(tab_storage_key) ?? "";
    } catch {
      return "";
    }
  }
  function remember_tab(id: string) {
    try {
      window.localStorage.setItem(tab_storage_key, id);
    } catch {
      // The last selected tab is only a convenience.
    }
  }
  let data = $state.raw<FlowData>({
    tab_id: "",
    layout: { nodes: [], viewport: null },
    relationships: [],
    tabs: [],
  });
  let tabs = $state.raw<FlowTab[]>([]);
  let tab_id = $state(recalled_tab());
  let tab_busy = $state(false);
  let renaming_tab_id = $state<string | null>(null);
  let deleting_tab = $state.raw<FlowTab | null>(null);
  let load_request = 0;
  let requested_tab = "";
  let nodes = $state.raw<CanvasNode[]>([]);
  let edges = $state.raw<RelationshipEdge[]>([]);
  let viewport = $state.raw<Viewport>({ x: 0, y: 0, zoom: 1 });
  let loaded = $state(false);
  let load_error = $state.raw<unknown>(null);
  let saving = $state(false);
  let add_open = $state(false);
  let selected_tasks = $state<string[]>([]);
  let selected_task_columns = $state<string[]>([]);
  let task_query = $state("");
  let canvas_archives = $state.raw<Pick<Archive, "task">[]>([]);
  let archive_loading = $state(false);
  let archive_error = $state.raw<unknown>(null);
  let canvas_ready = $state(false);
  let archive_request = 0;
  let notes = $state.raw<NotePreview[]>([]);
  let notes_loading = $state(false);
  let notes_error = $state.raw<unknown>(null);
  let notes_request = 0;
  let note_picker_open = $state(false);
  let note_query = $state("");
  let selected_note_ids = $state<number[]>([]);
  let editing_annotation_id = $state<string | null>(null);
  let insertion_position = $state.raw<{ x: number; y: number } | null>(null);
  let context_edge_id = $state<string | null>(null);
  // Keeps the edge menu while it closes after its line has been removed.
  let context_edge_snapshot = $state.raw<Relationship | null>(null);
  let context_menu_open = $state(false);
  function canvas_contextmenu(event: MouseEvent) {
    const edge =
      event.target instanceof Element
        ? event.target.closest(".svelte-flow__edge[data-id]")
        : null;
    context_edge_id = edge?.getAttribute("data-id") ?? null;
    context_edge_snapshot = relationship_of(context_edge_id);
    if (context_edge_id !== null) {
      selected_edges = [context_edge_id];
      return;
    }
    if (
      !(event.target instanceof Element) ||
      (!event.target.classList.contains("svelte-flow__pane") &&
        event.target !== event.currentTarget)
    ) {
      event.preventDefault();
      return;
    }
    if (event.target === event.currentTarget) {
      insertion_position = null;
      return;
    }
    const bounds = (event.currentTarget as HTMLElement).getBoundingClientRect();
    insertion_position = {
      x: (event.clientX - bounds.left - viewport.x) / viewport.zoom,
      y: (event.clientY - bounds.top - viewport.y) / viewport.zoom,
    };
  }
  const unplaced_notes = $derived(
    notes.filter(
      (note) =>
        !data.layout.annotations?.some(
          (entry) => entry.kind === "note" && entry.note_id === note.id,
        ),
    ),
  );
  const available_notes = $derived(
    unplaced_notes.filter((note) =>
      (note.title + "\n" + note.content)
        .toLocaleLowerCase()
        .includes(note_query.trim().toLocaleLowerCase()),
    ),
  );
  const selected_notes = $derived(
    unplaced_notes.filter((note) => selected_note_ids.includes(note.id)),
  );
  async function refresh_notes(after_import = false) {
    if (disposed || (import_blocked() && !after_import)) {
      return;
    }
    const request = ++notes_request;
    notes_loading = true;
    notes_error = null;
    try {
      const result = await invoke<NotePreview[]>("get_notes");
      if (!disposed && request === notes_request) {
        notes = result;
      }
    } catch (error) {
      if (!disposed && request === notes_request) {
        notes_error = error;
      }
    } finally {
      if (!disposed && request === notes_request) {
        notes_loading = false;
      }
    }
  }
  $effect(() => {
    void note_revision;
    untrack(() => void refresh_notes());
  });
  function edit_annotation(annotation: FlowAnnotation | null) {
    if (annotation?.kind === "note") {
      onOpenNote(annotation.note_id);
      return;
    }
    if (read_only || saving) {
      return;
    }
    editing_annotation_id = annotation?.id ?? null;
  }
  async function put_annotation(annotation: FlowAnnotation) {
    const layout = moved_layout(data.layout, nodes, viewport);
    const entries = layout.annotations ?? [];
    return save_layout({
      ...layout,
      annotations: [
        ...entries.filter((entry) => entry.id !== annotation.id),
        annotation,
      ],
    });
  }
  function new_annotation_position() {
    return {
      id: crypto.randomUUID(),
      x: insertion_position?.x ?? (80 - viewport.x) / viewport.zoom,
      y: insertion_position?.y ?? (80 - viewport.y) / viewport.zoom,
      width: 280,
    };
  }
  async function add_annotation() {
    const annotation: FlowAnnotation = {
      ...new_annotation_position(),
      kind: "text",
      text: "",
    };
    if (await put_annotation(annotation)) {
      editing_annotation_id = annotation.id;
    }
  }
  async function save_annotation_text(id: string, text: string) {
    const annotation = moved_layout(
      data.layout,
      nodes,
      viewport,
    ).annotations?.find((entry) => entry.id === id);
    if (!annotation || annotation.kind !== "text" || read_only || saving) {
      return false;
    }
    const saved = await put_annotation({ ...annotation, text });
    if (saved && editing_annotation_id === id) {
      editing_annotation_id = null;
    }
    return saved;
  }
  async function add_notes() {
    if (read_only || saving || !loaded || !selected_notes.length) {
      return;
    }
    const layout = moved_layout(data.layout, nodes, viewport);
    const saved = await save_layout({
      ...layout,
      annotations: [
        ...(layout.annotations ?? []),
        ...selected_notes.map((note, index): FlowAnnotation => {
          const position = new_annotation_position();
          return {
            ...position,
            x: position.x + (index % 3) * 320,
            y: position.y + Math.floor(index / 3) * 360,
            kind: "note",
            note_id: note.id,
          };
        }),
      ],
    });
    if (saved) {
      selected_note_ids = [];
      note_picker_open = false;
    }
  }
  let selected_edges = $state<string[]>([]);
  let disposed = false;
  let pending_layout: FlowLayout | null = null;
  let idle_waiters: (() => void)[] = [];
  function finish_saving() {
    saving = false;
    if (import_blocked()) {
      pending_layout = null;
    }
    if (pending_layout) {
      const next = { ...pending_layout, connections: data.layout.connections };
      pending_layout = null;
      if (disposed) {
        // The view closed while saving; still persist the last queued layout.
        void write_flow(() =>
          invoke("save_flow_layout", {
            expectedBoardId: board_id,
            tabId: tab_id,
            layout: next,
          }),
        ).catch((error) => {
          if (!import_blocked()) {
            toast.error(translateCommandError(error));
          }
        });
      } else {
        void save_layout(next);
      }
    }
    if (!saving) {
      const waiters = idle_waiters;
      idle_waiters = [];
      waiters.forEach((resolve) => resolve());
    }
  }
  // Tab changes wait for queued saves so layouts never cross tabs.
  function wait_idle() {
    return saving
      ? new Promise<void>((resolve) => idle_waiters.push(resolve))
      : Promise.resolve();
  }
  const visible_ids = $derived(
    nodes
      .filter((node) => !node.hidden)
      .map((node) => `${node.id}:${node.type === "task" && node.data.archived}`)
      .join(","),
  );
  const ariaLabelConfig = $derived({
    "node.a11yDescription.default": m.flow_node_keyboard(),
    "node.a11yDescription.keyboardDisabled": m.flow_node_readonly_keyboard(),
    "node.a11yDescription.ariaLiveMessage": ({
      x,
      y,
    }: {
      direction: string;
      x: number;
      y: number;
    }) => m.flow_node_moved({ x: formatNumber(x), y: formatNumber(y) }),
    "edge.a11yDescription.default": read_only
      ? m.flow_node_readonly_keyboard()
      : m.flow_edge_keyboard(),
    "controls.ariaLabel": m.flow_controls(),
    "controls.zoomIn.ariaLabel": m.flow_zoom_in(),
    "controls.zoomOut.ariaLabel": m.flow_zoom_out(),
    "controls.fitView.ariaLabel": m.flow_fit_view(),
    "controls.interactive.ariaLabel": m.flow_interactivity(),
    "minimap.ariaLabel": m.flow_minimap(),
    "handle.ariaLabel": m.flow_handle(),
  });
  function relationship_label(edge: Relationship) {
    const names = {
      source: task_title(edge.source),
      target: task_title(edge.target),
    };
    if (edge.source_arrow && edge.target_arrow) {
      return m.flow_edge_label_both(names);
    }
    if (edge.source_arrow || edge.target_arrow) {
      return edge.target_arrow
        ? m.flow_edge_label_arrow(names)
        : m.flow_edge_label_arrow({
            source: names.target,
            target: names.source,
          });
    }
    return m.flow_edge_label(names);
  }
  const relationship_of = (id: string | null) =>
    edges.find((edge) => edge.id === id)?.data?.relationship ?? null;
  const toolbar_relationship = $derived(
    selected_edges.length === 1 ? relationship_of(selected_edges[0]) : null,
  );
  const context_relationship = $derived(
    relationship_of(context_edge_id) ?? context_edge_snapshot,
  );
  function task_title(id: number) {
    const node = nodes.find((node) => node.id === `task_${id}`);
    return (
      (node?.type === "task" && node.data.task.title.trim()) ||
      m.task_untitled()
    );
  }
  function update_selection(selection: Edge[]) {
    const ids = selection.map((edge) => edge.id);
    // Selection callbacks also fire when projected arrays change.
    if (ids.join(",") !== selected_edges.join(",")) {
      selected_edges = ids;
    }
  }
  const canvas_ids = $derived(
    new Set(data.layout.nodes.map((node) => `task_${node.task_id}`)),
  );
  const available_columns = $derived(
    available_task_columns(columns, canvas_ids),
  );
  const archive_ids = $derived(
    missing_archive_task_ids(data.layout, columns).join(","),
  );
  $effect(() => {
    const ids = archive_ids;
    void columns;
    void archives;
    if (loaded) {
      untrack(
        () => void refresh_archives(ids ? ids.split(",").map(Number) : []),
      );
    }
  });
  async function refresh_archives(
    ids = missing_archive_task_ids(data.layout, columns),
    after_import = false,
  ) {
    if (disposed || (import_blocked() && !after_import)) {
      return;
    }
    const request = ++archive_request;
    archive_loading = ids.length > 0;
    archive_error = null;
    const cached = new Map(
      archives.map((archive) => [get_task_id(archive.task.id), archive.task]),
    );
    try {
      const missing = ids.filter((id) => !cached.has(id));
      const fetched = missing.length
        ? await invoke<TaskSerialized[]>("get_archive_tasks", {
            expectedBoardId: board_id,
            taskIds: missing,
          })
        : [];
      if (!disposed && request === archive_request) {
        for (const serialized of fetched) {
          const task = deserialize_task(serialized);
          cached.set(get_task_id(task.id), task);
        }
        // Tasks deleted since the canvas was saved are simply absent.
        canvas_archives = ids.flatMap((id) => {
          const task = cached.get(id);
          return task ? [{ task }] : [];
        });
      }
    } catch (error) {
      if (!disposed && request === archive_request) {
        archive_error = error;
      }
    } finally {
      if (!disposed && request === archive_request) {
        archive_loading = false;
        canvas_ready = true;
      }
    }
  }
  $effect(() => {
    const next: CanvasNode[] = build_nodes(
      data.layout,
      columns,
      {
        read_only: read_only || saving,
        locked: read_only,
        onOpen: (task) =>
          canvas_archives.some((archive) => archive.task.id === task.id)
            ? onViewArchivedTask(task)
            : onViewTask(task),
        onCopyLink: onCopyTaskLink,
        onArchive: async (task) => {
          if (await onArchiveTask(task)) {
            await refresh_archives();
          }
        },
        onDelete: onDeleteTask,
        onEdit: onEditTask,
        onRemove: remove_node,
        onScale: scale_nodes,
      },
      search_task_ids,
      canvas_archives,
    );
    next.push(
      ...build_annotations(data.layout, notes, {
        notes_unavailable: notes_loading || notes_error !== null,
        read_only: read_only || saving,
        locked: read_only,
        onEdit: edit_annotation,
        editing_id: editing_annotation_id,
        onSaveText: save_annotation_text,
        onCancelEdit: () => (editing_annotation_id = null),
        onRemove: remove_node,
        onScale: scale_nodes,
      }),
    );
    untrack(() => {
      const current = new Map(nodes.map((node) => [node.id, node]));
      nodes = next.map((node) => ({
        ...node,
        selected: current.get(node.id)?.selected,
      }));
    });
  });
  $effect(() => {
    void visible_ids;
    edges = build_edges(
      data.relationships,
      untrack(() => nodes),
      relationship_label,
      data.layout.connections,
    ).map((edge) => ({ ...edge, selected: selected_edges.includes(edge.id) }));
  });

  async function load(next = tab_id, after_import = false) {
    if (disposed || (import_blocked() && !after_import)) {
      return;
    }
    const request = ++load_request;
    clearTimeout(viewport_timer);
    viewport_timer = undefined;
    loaded = false;
    canvas_ready = false;
    archive_request++;
    load_error = null;
    pending_layout = null;
    editing_annotation_id = null;
    selected_edges = [];
    try {
      const result = await invoke<FlowData>("get_flow", {
        expectedBoardId: board_id,
        tabId: next || null,
      });
      if (disposed || request !== load_request) {
        return;
      }
      data = result;
      tabs = result.tabs;
      tab_id = result.tab_id;
      remember_tab(result.tab_id);
      viewport = result.layout.viewport ?? { x: 0, y: 0, zoom: 1 };
      loaded = true;
    } catch (error) {
      if (!disposed && request === load_request) {
        load_error = error;
      }
    }
  }
  // `tab_id` changes only after loading, so queued saves keep their original tab.
  async function switch_tab(id: string) {
    if (import_blocked()) {
      return;
    }
    requested_tab = id;
    if (id === tab_id && loaded) {
      return;
    }
    flush_viewport_save();
    await wait_idle();
    if (!disposed && !import_blocked() && id === requested_tab) {
      await load(id);
    }
  }
  async function change_tabs(request: () => Promise<FlowTab[]>) {
    if (read_only || tab_busy || import_blocked()) {
      return false;
    }
    tab_busy = true;
    try {
      const result = await request();
      if (disposed || import_blocked()) {
        return false;
      }
      tabs = result;
      return true;
    } catch (error) {
      if (disposed || import_blocked()) {
        return false;
      }
      toast.error(translateCommandError(error));
      if (parseCommandError(error)?.code === "FLOW_TAB_NOT_FOUND") {
        void switch_tab("");
      }
      return false;
    } finally {
      tab_busy = false;
    }
  }
  async function create_tab() {
    const id = crypto.randomUUID();
    const name = m.flow_tab_new_name({ number: formatNumber(tabs.length + 1) });
    if (
      await change_tabs(() =>
        write_flow(() =>
          invoke<FlowTab[]>("create_flow_tab", {
            expectedBoardId: board_id,
            tabId: id,
            name,
          }),
        ),
      )
    ) {
      await switch_tab(id);
      if (tab_id === id) {
        renaming_tab_id = id;
      }
    }
  }
  async function rename_tab(id: string, name: string) {
    if (tabs.find((tab) => tab.id === id)?.name === name.trim()) {
      return true;
    }
    return change_tabs(() =>
      write_flow(() =>
        invoke<FlowTab[]>("rename_flow_tab", {
          expectedBoardId: board_id,
          tabId: id,
          name,
        }),
      ),
    );
  }
  // Show the new order immediately so dropped tabs do not snap back while saving.
  async function reorder_tabs(ids: string[]) {
    if (read_only || tab_busy) {
      return;
    }
    const previous = tabs;
    tabs = ids.flatMap((id) => previous.filter((tab) => tab.id === id));
    if (
      !(await change_tabs(() =>
        write_flow(() =>
          invoke<FlowTab[]>("reorder_flow_tabs", {
            expectedBoardId: board_id,
            tabIds: ids,
          }),
        ),
      ))
    ) {
      tabs = previous;
    }
  }
  function move_tab(id: string, offset: number) {
    const ids = moved_tab_ids(tabs, id, offset);
    if (ids) {
      void reorder_tabs(ids);
    }
  }
  async function delete_tab(id: string) {
    const next = tab_after_delete(tabs, id);
    if (!next) {
      return;
    }
    if (id === tab_id) {
      clearTimeout(viewport_timer);
      viewport_timer = undefined;
      await wait_idle();
    }
    if (
      (await change_tabs(() =>
        write_flow(() =>
          invoke<FlowTab[]>("delete_flow_tab", {
            expectedBoardId: board_id,
            tabId: id,
          }),
        ),
      )) &&
      id === tab_id
    ) {
      await switch_tab(next);
    }
  }
  function pause_import() {
    import_paused = true;
    clearTimeout(viewport_timer);
    viewport_timer = undefined;
    pending_layout = null;
    load_request++;
    notes_request++;
    archive_request++;
    requested_tab = "";
    loaded = false;
    canvas_ready = false;
    data = {
      tab_id: "",
      layout: { nodes: [], viewport: null },
      relationships: [],
      tabs: [],
    };
    tabs = [];
    nodes = [];
    edges = [];
    notes = [];
    canvas_archives = [];
    selected_edges = [];
    editing_annotation_id = null;
    add_open = false;
    note_picker_open = false;
    renaming_tab_id = null;
    deleting_tab = null;
  }
  onMount(() => {
    const unregister = flow_imports.register(board_id, {
      pause: pause_import,
      reload: async () => {
        if (disposed) {
          return;
        }
        await Promise.all([load("", true), refresh_notes(true)]);
        if (!disposed && loaded) {
          await refresh_archives(undefined, true);
        }
      },
      resume: () => {
        import_paused = false;
      },
    });
    void load();
    const on_focus = () => void refresh_notes();
    window.addEventListener("focus", on_focus);
    const listener = listen<{ kind?: string }>(
      "cardbe:data-changed",
      (event) => {
        if (event.payload.kind === "note") {
          void refresh_notes();
        }
      },
    );
    return () => {
      window.removeEventListener("focus", on_focus);
      void listener.then((unlisten) => unlisten());
      flush_viewport_save();
      disposed = true;
      unregister();
    };
  });

  // Panning and zooming save only after the viewport settles.
  const viewport_save_delay = 800;
  let viewport_timer: ReturnType<typeof setTimeout> | undefined;
  function schedule_viewport_save() {
    clearTimeout(viewport_timer);
    if (import_blocked()) {
      viewport_timer = undefined;
      return;
    }
    viewport_timer = setTimeout(flush_viewport_save, viewport_save_delay);
  }
  function flush_viewport_save() {
    if (viewport_timer === undefined) {
      return;
    }
    clearTimeout(viewport_timer);
    viewport_timer = undefined;
    if (import_blocked()) {
      return;
    }
    const saved = data.layout.viewport;
    if (
      saved?.x !== viewport.x ||
      saved.y !== viewport.y ||
      saved.zoom !== viewport.zoom
    ) {
      save_positions();
    }
  }
  async function save_layout(layout: FlowLayout) {
    // Every layout save carries the current viewport.
    clearTimeout(viewport_timer);
    viewport_timer = undefined;
    if (read_only || !loaded || disposed || import_blocked()) {
      return false;
    }
    if (saving) {
      pending_layout = layout;
      return false;
    }
    data = { ...data, layout };
    saving = true;
    try {
      await write_flow(() =>
        invoke("save_flow_layout", {
          expectedBoardId: board_id,
          tabId: tab_id,
          layout,
        }),
      );
      if (!disposed && !import_blocked()) {
        data = { ...data, layout };
      }
      return !disposed && !import_blocked();
    } catch (error) {
      pending_layout = null;
      if (import_blocked()) {
        return false;
      }
      toast.error(translateCommandError(error));
      if (!disposed) {
        await load();
      }
      return false;
    } finally {
      finish_saving();
    }
  }
  function save_positions() {
    void save_layout(moved_layout(data.layout, nodes, viewport));
  }
  function save_keyboard_position(event: KeyboardEvent) {
    if (
      event.key.startsWith("Arrow") &&
      event.target instanceof HTMLElement &&
      event.target.classList.contains("svelte-flow__node")
    ) {
      save_positions();
    }
  }
  async function add_nodes(ids: string[]) {
    if (read_only || saving || !loaded) {
      return;
    }
    const active_ids = new Set(
      columns.flatMap((column) => column.tasks.map((task) => task.id)),
    );
    const saved = await save_layout(
      added_layout(
        moved_layout(data.layout, nodes, viewport),
        ids.filter((id) => active_ids.has(id)),
        viewport,
        insertion_position ?? undefined,
      ),
    );
    if (saved) {
      selected_tasks = [];
      selected_task_columns = [];
      add_open = false;
    }
  }
  // Removal keeps unsaved movement of the remaining items.
  function without_nodes(ids: ReadonlySet<string>): FlowLayout {
    const layout = pending_layout ?? moved_layout(data.layout, nodes, viewport);
    return {
      ...layout,
      nodes: layout.nodes.filter((node) => !ids.has(`task_${node.task_id}`)),
      annotations: (layout.annotations ?? []).filter(
        (entry) => !ids.has(`annotation_${entry.id}`),
      ),
    };
  }
  // Menu choices on a selected card resize the whole selection; dragging resizes one card.
  function scale_nodes(
    id: string,
    change: (scale: number) => number,
    selection: boolean,
  ) {
    const selected = nodes
      .filter((node) => node.selected)
      .map((node) => node.id);
    const ids = new Set(selection && selected.includes(id) ? selected : [id]);
    // Steps made during a save build on the queued layout, so none are lost.
    void save_layout(
      scaled_layout(
        pending_layout ?? moved_layout(data.layout, nodes, viewport),
        ids,
        change,
      ),
    );
  }
  function remove_node(id: string) {
    void save_layout(without_nodes(new Set([id])));
  }
  async function create_relationship(connection: Connection) {
    if (read_only || saving || !loaded) {
      return;
    }
    saving = true;
    try {
      const relationship = await write_flow(() =>
        invoke<Relationship>("create_task_relationship", {
          expectedBoardId: board_id,
          relationship: {
            source: get_task_id(connection.source),
            target: get_task_id(connection.target),
            // New lines point to the task they were dragged to.
            source_arrow: false,
            target_arrow: true,
          },
        }),
      );
      if (!disposed && !import_blocked()) {
        data = {
          ...data,
          relationships: [...data.relationships, relationship],
        };
        const layout = connected_layout(
          pending_layout ?? moved_layout(data.layout, nodes, viewport),
          relationship,
          connection,
        );
        data = { ...data, layout };
        pending_layout = layout;
      }
    } catch (error) {
      if (import_blocked()) {
        return;
      }
      toast.error(translateCommandError(error));
    } finally {
      finish_saving();
    }
  }
  async function change_arrows(
    current: Relationship,
    arrows: Partial<Pick<Relationship, "source_arrow" | "target_arrow">>,
  ) {
    if (read_only || saving || !loaded) {
      return;
    }
    saving = true;
    try {
      const saved = await write_flow(() =>
        invoke<Relationship>("change_task_relationship", {
          expectedBoardId: board_id,
          relationship: { ...current, ...arrows },
        }),
      );
      if (!disposed && !import_blocked()) {
        data = {
          ...data,
          relationships: data.relationships.map((edge) =>
            relationship_id(edge) === relationship_id(saved) ? saved : edge,
          ),
          layout: moved_layout(data.layout, nodes, viewport),
        };
      }
    } catch (error) {
      if (import_blocked()) {
        return;
      }
      toast.error(translateCommandError(error));
      if (
        !disposed &&
        parseCommandError(error)?.code === "RELATIONSHIP_NOT_FOUND"
      ) {
        await load();
      }
    } finally {
      finish_saving();
    }
  }
  async function remove_relationships(ids: string[]) {
    if (read_only || saving || !loaded) {
      return;
    }
    saving = true;
    try {
      for (const id of ids) {
        if (import_blocked()) {
          return;
        }
        const relationship = edges.find((edge) => edge.id === id)?.data
          ?.relationship;
        if (!relationship) {
          continue;
        }
        await write_flow(() =>
          invoke("remove_task_relationship", {
            expectedBoardId: board_id,
            relationship,
          }),
        );
        if (!disposed && !import_blocked()) {
          data = {
            ...data,
            layout: {
              ...data.layout,
              connections: Object.fromEntries(
                Object.entries(data.layout.connections ?? {}).filter(
                  ([key]) => key !== id,
                ),
              ),
            },
            relationships: data.relationships.filter(
              (edge) => relationship_id(edge) !== id,
            ),
          };
        }
      }
      selected_edges = [];
    } catch (error) {
      if (import_blocked()) {
        return;
      }
      toast.error(translateCommandError(error));
      if (!disposed) {
        await load();
      }
    } finally {
      finish_saving();
    }
  }
  const before_delete: OnBeforeDelete<CanvasNode, Edge> = async ({
    nodes: removed,
    edges: removed_edges,
  }) => {
    if (removed.length) {
      await save_layout(without_nodes(new Set(removed.map((node) => node.id))));
    } else {
      await remove_relationships(removed_edges.map((edge) => edge.id));
    }
    // Canvas removal never deletes tasks or their logical relationships.
    return false;
  };
</script>

<div class="flow-canvas flex h-full min-h-0 flex-col bg-background">
  <div class="flex items-center gap-2 border-b px-3 py-2">
    <DropdownMenu.Root>
      <DropdownMenu.Trigger
        class={buttonVariants({ size: "sm", variant: "outline" })}
        disabled={read_only || saving || !loaded}
        ><PlusIcon />{m.flow_add_to_canvas()}<ChevronDownIcon
        /></DropdownMenu.Trigger
      >
      <DropdownMenu.Content align="start">
        <DropdownMenu.Item
          disabled={read_only || saving || !loaded}
          onclick={() => {
            insertion_position = null;
            add_open = true;
          }}><ListPlusIcon />{m.flow_add_tasks()}</DropdownMenu.Item
        >
        <DropdownMenu.Item
          disabled={read_only || saving || !loaded}
          onclick={() => {
            insertion_position = null;
            note_query = "";
            selected_note_ids = [];
            note_picker_open = true;
            void refresh_notes();
          }}><FileTextIcon />{m.flow_add_note()}</DropdownMenu.Item
        >
        <DropdownMenu.Separator />
        <DropdownMenu.Item
          disabled={read_only || saving || !loaded}
          onSelect={() => {
            insertion_position = null;
            void add_annotation();
          }}><TypeIcon />{m.flow_add_annotation()}</DropdownMenu.Item
        >
      </DropdownMenu.Content>
    </DropdownMenu.Root>
    {#if tabs.length}
      <Separator
        orientation="vertical"
        class="data-[orientation=vertical]:h-5"
      />
      <FlowTabs
        {tabs}
        active_id={tab_id}
        panel_id="flow-tab-panel"
        {read_only}
        busy={tab_busy || import_paused}
        bind:renaming_id={renaming_tab_id}
        onSelect={(id) => void switch_tab(id)}
        onCreate={create_tab}
        onRename={rename_tab}
        onMove={move_tab}
        onReorder={(ids) => void reorder_tabs(ids)}
        onDelete={(id) =>
          (deleting_tab = tabs.find((tab) => tab.id === id) ?? null)}
      />
    {/if}
    {#if toolbar_relationship}
      {@const relationship = toolbar_relationship}
      <DropdownMenu.Root>
        <DropdownMenu.Trigger
          class={buttonVariants({ size: "sm", variant: "ghost" })}
          disabled={read_only || saving}
          >{m.flow_arrows()}<ChevronDownIcon /></DropdownMenu.Trigger
        >
        <DropdownMenu.Content align="start" class="max-w-80">
          <DropdownMenu.CheckboxItem
            checked={relationship.target_arrow}
            closeOnSelect={false}
            disabled={saving}
            onCheckedChange={(checked) =>
              void change_arrows(relationship, { target_arrow: checked })}
            ><span class="truncate"
              >{m.flow_arrow_to({
                name: task_title(relationship.target),
              })}</span
            ></DropdownMenu.CheckboxItem
          >
          <DropdownMenu.CheckboxItem
            checked={relationship.source_arrow}
            closeOnSelect={false}
            disabled={saving}
            onCheckedChange={(checked) =>
              void change_arrows(relationship, { source_arrow: checked })}
            ><span class="truncate"
              >{m.flow_arrow_to({
                name: task_title(relationship.source),
              })}</span
            ></DropdownMenu.CheckboxItem
          >
        </DropdownMenu.Content>
      </DropdownMenu.Root>
    {/if}
    {#if selected_edges.length > 0}
      <Button
        size="sm"
        variant="ghost"
        class="text-destructive hover:bg-destructive/10 hover:text-destructive dark:hover:bg-destructive/20"
        disabled={read_only || saving}
        onclick={() => remove_relationships(selected_edges)}
        ><UnlinkIcon />{m.flow_disconnect()}</Button
      >
    {/if}
    <div class="ml-auto flex shrink-0 items-center gap-2">
      {#if saving || archive_loading}
        <span class="text-xs text-muted-foreground" role="status">
          {saving ? m.flow_saving() : m.board_loading()}
        </span>
      {/if}
      <Popover.Root>
        <Popover.Trigger
          class={buttonVariants({ size: "icon-sm", variant: "ghost" })}
          aria-label={m.flow_canvas_help()}
          title={m.flow_canvas_help()}><CircleHelpIcon /></Popover.Trigger
        >
        <Popover.Content align="end" class="w-72 space-y-2 text-sm">
          <p class="font-medium">{m.flow_canvas_help()}</p>
          <p class="whitespace-pre-line leading-7 text-muted-foreground">
            {m.flow_help()}
          </p>
          <p class="text-xs text-muted-foreground">{m.flow_local()}</p>
        </Popover.Content>
      </Popover.Root>
    </div>
  </div>
  <div
    id="flow-tab-panel"
    class="flex min-h-0 flex-1 flex-col"
    role={tabs.length ? "tabpanel" : undefined}
    aria-labelledby={tabs.length ? `flow-tab-${tab_id}` : undefined}
  >
    {#if archive_error}
      <div
        role="alert"
        class="flex items-center gap-2 px-3 text-sm text-destructive"
      >
        <span>{translateCommandError(archive_error)}</span>
        <Button size="sm" variant="link" onclick={() => refresh_archives()}
          >{m.board_search_retry()}</Button
        >
      </div>
    {/if}
    {#if notes_error}
      <div
        role="alert"
        class="flex items-center gap-2 px-3 text-sm text-destructive"
      >
        <span>{translateCommandError(notes_error)}</span>
        <Button size="sm" variant="link" onclick={() => refresh_notes()}
          >{m.note_retry()}</Button
        >
      </div>
    {/if}
    <ContextMenu.Root bind:open={context_menu_open}>
      <ContextMenu.Trigger
        class="relative min-h-0 flex-1"
        tabindex={0}
        aria-label={m.workspace_flow()}
        aria-haspopup="menu"
        disabled={read_only || !loaded}
        oncontextmenu={canvas_contextmenu}
      >
        {#if loaded && canvas_ready}
          <SvelteFlow
            bind:nodes
            bind:edges
            bind:viewport
            {nodeTypes}
            {edgeTypes}
            connectionMode={ConnectionMode.Loose}
            proOptions={{ hideAttribution: true }}
            {ariaLabelConfig}
            defaultMarkerColor="var(--muted-foreground)"
            onkeyup={save_keyboard_position}
            minZoom={0.1}
            maxZoom={4}
            fitView={data.layout.viewport === null}
            fitViewOptions={{ padding: 0.2, minZoom: 0.1, maxZoom: 1 }}
            panOnScroll
            zoomOnScroll={false}
            panOnDrag={[1]}
            panActivationKey={null}
            selectionOnDrag
            selectionKey={null}
            selectionMode={SelectionMode.Partial}
            onlyRenderVisibleElements
            nodesDraggable={!read_only && !saving}
            nodesConnectable={!read_only && !saving}
            deleteKey={read_only || saving ? null : ["Backspace", "Delete"]}
            onnodedragstop={save_positions}
            onselectiondragstop={save_positions}
            onmoveend={schedule_viewport_save}
            onbeforeconnect={(connection) => {
              void create_relationship(connection);
              return false;
            }}
            onbeforedelete={before_delete}
            onselectionchange={({ edges: selection }) =>
              update_selection(selection)}
          >
            <Background patternColor="var(--border)" />
            <Controls
              showLock={false}
              fitViewOptions={{ padding: 0.2, duration: 200 }}
            />
          </SvelteFlow>
          {#if nodes.length === 0 && !archive_loading && !archive_error}
            <div
              class="pointer-events-none absolute inset-0 grid place-items-center p-6"
            >
              <Empty.Root class="bg-card/90">
                <Empty.Header
                  ><Empty.Title>{m.flow_empty()}</Empty.Title><Empty.Description
                    >{m.flow_empty_description()}</Empty.Description
                  ></Empty.Header
                >
                <Empty.Content
                  ><Button
                    class="pointer-events-auto"
                    disabled={read_only || saving}
                    onclick={() => {
                      insertion_position = null;
                      add_open = true;
                    }}>{m.flow_add_tasks()}</Button
                  ></Empty.Content
                >
              </Empty.Root>
            </div>
          {/if}
        {:else if load_error}
          <div class="grid h-full place-content-center gap-3 p-6 text-center">
            <p>{translateCommandError(load_error)}</p>
            <Button onclick={() => load()}>{m.board_search_retry()}</Button>
          </div>
        {:else}<div
            class="grid h-full place-items-center text-muted-foreground"
          >
            {m.board_loading()}
          </div>{/if}
      </ContextMenu.Trigger>
      <ContextMenu.Content class="max-w-80">
        {#if context_relationship}
          {@const relationship = context_relationship}
          <ContextMenu.CheckboxItem
            checked={relationship.target_arrow}
            closeOnSelect={false}
            disabled={saving}
            onCheckedChange={(checked) =>
              void change_arrows(relationship, { target_arrow: checked })}
            ><span class="truncate"
              >{m.flow_arrow_to({
                name: task_title(relationship.target),
              })}</span
            ></ContextMenu.CheckboxItem
          >
          <ContextMenu.CheckboxItem
            checked={relationship.source_arrow}
            closeOnSelect={false}
            disabled={saving}
            onCheckedChange={(checked) =>
              void change_arrows(relationship, { source_arrow: checked })}
            ><span class="truncate"
              >{m.flow_arrow_to({
                name: task_title(relationship.source),
              })}</span
            ></ContextMenu.CheckboxItem
          >
          <ContextMenu.Separator />
          <ContextMenu.Item
            variant="destructive"
            disabled={saving}
            onclick={() => {
              context_menu_open = false;
              void remove_relationships([relationship_id(relationship)]);
            }}><UnlinkIcon />{m.flow_disconnect()}</ContextMenu.Item
          >
        {:else}
          <ContextMenu.Item
            disabled={read_only || saving || !loaded}
            onclick={() => (add_open = true)}
            ><ListPlusIcon />{m.flow_add_tasks()}</ContextMenu.Item
          >
          <ContextMenu.Item
            disabled={read_only || saving || !loaded}
            onclick={() => {
              note_query = "";
              selected_note_ids = [];
              note_picker_open = true;
              void refresh_notes();
            }}><FileTextIcon />{m.flow_add_note()}</ContextMenu.Item
          >
          <ContextMenu.Separator />
          <ContextMenu.Item
            disabled={read_only || saving || !loaded}
            onSelect={() => void add_annotation()}
            ><TypeIcon />{m.flow_add_annotation()}</ContextMenu.Item
          >
        {/if}
      </ContextMenu.Content>
    </ContextMenu.Root>
  </div>
</div>

<AlertDialog.Root
  open={deleting_tab !== null}
  onOpenChange={(open) => {
    if (!open) {
      deleting_tab = null;
    }
  }}
>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title
        >{m.flow_tab_delete_title({
          name: deleting_tab?.name.trim() || m.flow_tab_default(),
        })}</AlertDialog.Title
      >
      <AlertDialog.Description
        >{m.flow_tab_delete_description()}</AlertDialog.Description
      >
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action
        variant="destructive"
        onclick={() => {
          const tab = deleting_tab;
          deleting_tab = null;
          if (tab) {
            void delete_tab(tab.id);
          }
        }}>{m.common_delete()}</AlertDialog.Action
      >
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<Dialog.Root bind:open={add_open}>
  <Dialog.Content
    class="flex max-h-[calc(100dvh-4rem)] flex-col overflow-hidden"
  >
    <Dialog.Header
      ><Dialog.Title>{m.flow_add_tasks()}</Dialog.Title><Dialog.Description
        >{m.flow_add_description()}</Dialog.Description
      ></Dialog.Header
    >
    <Field.Group class="min-h-0 gap-4">
      <TaskSelectionPicker
        columns={available_columns}
        bind:query={task_query}
        bind:selected_column_ids={selected_task_columns}
        bind:selected_task_ids={selected_tasks}
        disabled={read_only || saving || !loaded}
      />

      <Button
        disabled={!selected_tasks.length || read_only || saving || !loaded}
        onclick={() => add_nodes(selected_tasks)}
        >{m.flow_add_selected({
          count: formatNumber(selected_tasks.length),
        })}</Button
      >
    </Field.Group>
  </Dialog.Content>
</Dialog.Root>

<Dialog.Root bind:open={note_picker_open}>
  <Dialog.Content class="gap-0 overflow-hidden p-0 sm:max-w-xl">
    <Dialog.Header class="px-6 pt-6 pb-4 text-start"
      ><Dialog.Title>{m.flow_add_note()}</Dialog.Title><Dialog.Description
        >{m.flow_note_description()}</Dialog.Description
      ></Dialog.Header
    >
    <div class="px-6 pb-4">
      <InputGroup.Root>
        <InputGroup.Input
          aria-label={m.note_search()}
          placeholder={m.note_search()}
          bind:value={note_query}
        />
        <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
      </InputGroup.Root>
    </div>
    <ScrollArea class="h-80 border-t">
      <div class="p-2">
        {#if notes_loading}<p
            class="p-8 text-center text-sm text-muted-foreground"
            role="status"
          >
            {m.note_loading()}
          </p>
        {:else if notes_error}<p
            class="px-4 pt-6 text-sm text-destructive"
            role="alert"
          >
            {translateCommandError(notes_error)}
          </p>
          <Button
            class="mx-4 mt-3"
            variant="outline"
            onclick={() => refresh_notes()}>{m.note_retry()}</Button
          >
        {:else}
          {#each available_notes as note (note.id)}<label
              class={`flex cursor-pointer items-center gap-3 rounded-md px-4 py-2.5 text-left hover:bg-muted/50 ${selected_note_ids.includes(note.id) ? "bg-accent/50" : ""}`}
              ><Checkbox
                disabled={read_only || saving || !loaded}
                checked={selected_note_ids.includes(note.id)}
                aria-label={note.title.trim() || m.note_untitled()}
                onCheckedChange={(checked) => {
                  selected_note_ids = selected_note_ids.filter(
                    (id) => id !== note.id,
                  );
                  if (checked === true) {
                    selected_note_ids = [...selected_note_ids, note.id];
                  }
                }}
              /><FileTextIcon class="size-4 shrink-0 text-muted-foreground" />
              <span class="min-w-0 flex-1 truncate text-sm font-medium">
                {note.title.trim() || m.note_untitled()}
              </span>
            </label>
          {:else}<div
              class="flex flex-col items-center gap-3 px-4 py-12 text-center text-muted-foreground"
            >
              <FileTextIcon class="size-8 opacity-50" />
              <p class="text-sm">
                {note_query.trim()
                  ? m.note_no_matching()
                  : m.flow_no_available_notes()}
              </p>
            </div>{/each}
        {/if}
      </div>
    </ScrollArea>
    <Dialog.Footer class="border-t px-6 py-4">
      <Button
        disabled={!selected_notes.length ||
          notes_loading ||
          notes_error !== null ||
          read_only ||
          saving ||
          !loaded}
        onclick={add_notes}
      >
        {m.flow_add_selected_notes({
          count: formatNumber(selected_notes.length),
        })}
      </Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>

<style>
  .flow-canvas {
    --xy-background-color: var(--background);
    --xy-edge-stroke: var(--muted-foreground);
    --xy-edge-stroke-selected: var(--primary);
    --xy-edge-label-color: var(--foreground);
    --xy-edge-label-background-color: var(--background);
    --xy-controls-button-background-color: var(--card);
    --xy-controls-button-background-color-hover: var(--accent);
    --xy-controls-button-color: var(--card-foreground);
    --xy-controls-button-border-color: var(--border);
    --xy-handle-background-color: var(--primary);
    --xy-handle-border-color: var(--background);
    --xy-selection-background-color: color-mix(
      in srgb,
      var(--primary) 12%,
      transparent
    );
    --xy-selection-border: 1px solid var(--primary);
  }
  .flow-canvas :global(.svelte-flow__node:focus-visible) {
    outline: 2px solid var(--ring);
    outline-offset: 3px;
  }
  .flow-canvas :global(.svelte-flow__pane.selection),
  .flow-canvas :global(.svelte-flow__node.draggable),
  .flow-canvas :global(.svelte-flow__nodesselection-rect) {
    cursor: default;
  }
  .flow-canvas :global(.svelte-flow__pane.selection.dragging),
  .flow-canvas :global(.svelte-flow__node.draggable.dragging),
  .flow-canvas :global(.svelte-flow__nodesselection-rect:active) {
    cursor: grabbing;
  }
  .flow-canvas :global(.svelte-flow__edge-label) {
    padding: 0 0.25rem;
    font-size: 0.6875rem;
    line-height: 1rem;
    color: var(--muted-foreground);
  }
  .flow-canvas :global(.svelte-flow__handle.connectionindicator) {
    cursor: pointer;
  }
</style>
