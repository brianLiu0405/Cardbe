import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile } from "@tauri-apps/plugin-fs";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { toast } from "svelte-sonner";
import { isPermissionGranted } from "@tauri-apps/plugin-notification";
import type { TaskExplorerQuery } from "./utils/task-explorer";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vite-plus/test";
import {
  applyLanguagePreference,
  getLocale,
  language,
} from "#lib/i18n/index.js";
import {
  getLocale as runtimeGetLocale,
  overwriteGetLocale,
} from "#lib/paraglide/runtime.js";
import { translateCommandError } from "#lib/command-errors.js";

import { BoardStore } from "./board.svelte";
import { flow_imports } from "./components/flow/import-coordinator";
import { create_task } from "./type/task.svelte";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ emit: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(), save: vi.fn() }));
vi.mock("@tauri-apps/plugin-fs", () => ({
  readTextFile: vi.fn(),
  writeTextFile: vi.fn(),
}));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn(),
  requestPermission: vi.fn(),
}));
vi.mock("svelte-sonner", () => ({
  toast: {
    error: vi.fn(),
    success: vi.fn(),
    warning: vi.fn(),
  },
}));

const invoke_mock = vi.mocked(invoke);

describe("BoardStore disposal", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("removes recurring work and listeners and prevents delayed initialization from restarting them", async () => {
    vi.useFakeTimers();
    const dev_window = new EventTarget();
    Object.assign(dev_window, { setInterval, clearInterval });
    const dev_document = new EventTarget();
    Object.assign(dev_document, { visibilityState: "visible" });
    vi.stubGlobal("window", dev_window);
    vi.stubGlobal("document", dev_document);
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) => {
      if (command === "get_settings") {
        return Promise.resolve({ notify_enabled: true });
      }
      if (command === "take_recovery_messages") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    vi.mocked(isPermissionGranted).mockResolvedValue(true);
    const store = new BoardStore();
    vi.spyOn(store, "get_boards").mockResolvedValue(undefined);
    vi.spyOn(store, "update_labels").mockImplementation(() => {});
    vi.spyOn(store, "get_columns").mockImplementation(() => {});
    vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
    const remove_online = vi.spyOn(dev_window, "removeEventListener");
    const remove_visibility = vi.spyOn(dev_document, "removeEventListener");
    await store.init();
    // Only Iroh sync; the backend schedules due notifications.
    expect(vi.getTimerCount()).toBe(1);
    const boards_ready = deferred<void>();
    vi.mocked(store.get_boards).mockReturnValueOnce(boards_ready.promise);
    const initializing = store.init();
    store.dispose();
    store.dispose();
    expect(vi.getTimerCount()).toBe(0);
    expect(remove_online).toHaveBeenCalledWith("online", expect.any(Function));
    expect(remove_visibility).toHaveBeenCalledWith(
      "visibilitychange",
      expect.any(Function),
    );
    invoke_mock.mockClear();
    dev_window.dispatchEvent(new Event("online"));
    dev_document.dispatchEvent(new Event("visibilitychange"));
    boards_ready.resolve();
    await initializing;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(vi.getTimerCount()).toBe(0);
    expect(invoke_mock.mock.calls.map(([command]) => command)).toEqual([
      "get_settings",
      "take_recovery_messages",
    ]);
  });

  it("turns notifications off when permission was revoked while the app ran", async () => {
    const dev_window = new EventTarget();
    Object.assign(dev_window, { setInterval, clearInterval });
    const dev_document = new EventTarget();
    Object.assign(dev_document, { visibilityState: "visible" });
    vi.stubGlobal("window", dev_window);
    vi.stubGlobal("document", dev_document);
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) => {
      if (command === "get_settings") {
        return Promise.resolve({ notify_enabled: true });
      }
      if (command === "take_recovery_messages") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    vi.mocked(isPermissionGranted).mockResolvedValue(true);
    vi.mocked(toast.warning).mockClear();
    const store = new BoardStore();
    vi.spyOn(store, "get_boards").mockResolvedValue(undefined);
    vi.spyOn(store, "update_labels").mockImplementation(() => {});
    vi.spyOn(store, "get_columns").mockImplementation(() => {});
    vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
    await store.init();
    expect(store.notify_enabled).toBe(true);

    vi.mocked(isPermissionGranted).mockResolvedValue(false);
    dev_window.dispatchEvent(new Event("focus"));
    await vi.waitFor(() => expect(store.notify_enabled).toBe(false));
    expect(invoke_mock).toHaveBeenCalledWith("set_notify_enabled", {
      enabled: false,
    });
    expect(toast.warning).toHaveBeenCalledTimes(1);

    store.dispose();
    invoke_mock.mockClear();
    dev_window.dispatchEvent(new Event("focus"));
    expect(invoke_mock).not.toHaveBeenCalled();
  });
});

it("keeps the structured error reason and retry action for failed reads", async () => {
  invoke_mock.mockReset();
  vi.mocked(toast.error).mockClear();
  const error = {
    code: "STALE_BOARD_REQUEST",
    detail: "private database path",
  };
  invoke_mock.mockRejectedValue(error);
  const store = create_store();
  store.update_labels();
  await vi.waitFor(() =>
    expect(toast.error).toHaveBeenCalledWith(
      expect.any(String),
      expect.objectContaining({
        description: translateCommandError(error),
        action: expect.objectContaining({ onClick: expect.any(Function) }),
      }),
    ),
  );
});

describe("cross-board task moves", () => {
  it("keeps the selected board and refreshes all tasks when move summaries arrive after a board switch", async () => {
    invoke_mock.mockReset();
    const store = create_store();
    const summaries = deferred<{
      boards: typeof store.boards;
      active_board_id: number;
    }>();
    invoke_mock.mockImplementation((command) => {
      if (command === "list_all_tasks") {
        return Promise.resolve({ items: [], next_cursor: null });
      }
      if (command === "get_boards") {
        return summaries.promise;
      }
      if (
        command === "get_columns" ||
        command === "get_labels" ||
        command === "get_task_templates" ||
        command === "search_tasks"
      ) {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    await store.search_all_tasks({
      query: "",
      board_ids: null,
      column_id: null,
      status: "active",
      sort: "title",
      due: "all",
      due_start: null,
      due_end: null,
      archive_start: null,
      archive_end: null,
      now: Date.now(),
    });
    const moving = store.move_task_to_board("task_42", 8, 3);
    await vi.waitFor(() =>
      expect(invoke_mock).toHaveBeenCalledWith("get_boards"),
    );
    await expect(store.switch_board(9)).resolves.toBe(true);
    summaries.resolve({ boards: store.boards, active_board_id: 7 });
    await expect(moving).resolves.toBe(true);
    expect(store.active_board_id).toBe(9);
    expect(
      invoke_mock.mock.calls.filter(
        ([command]) => command === "list_all_tasks",
      ),
    ).toHaveLength(2);
  });

  it("preserves undo for an edit completed while board summaries are refreshing", async () => {
    invoke_mock.mockReset();
    const store = create_store();
    store.can_undo = true;
    const summaries = deferred<{
      boards: typeof store.boards;
      active_board_id: number;
    }>();
    invoke_mock.mockImplementation((command) => {
      if (command === "get_boards") {
        return summaries.promise;
      }
      if (
        command === "get_columns" ||
        command === "get_labels" ||
        command === "search_tasks"
      ) {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const moving = store.move_task_to_board("task_42", 8, 3);
    await vi.waitFor(() =>
      expect(invoke_mock).toHaveBeenCalledWith("get_boards"),
    );
    expect(store.can_undo).toBe(false);
    store.can_undo = true;
    summaries.resolve({ boards: store.boards, active_board_id: 7 });
    await expect(moving).resolves.toBe(true);
    expect(store.can_undo).toBe(true);
  });

  it("passes explicit source and destination IDs and clears undo after success", async () => {
    invoke_mock.mockReset();
    const store = create_store();
    store.can_undo = true;
    invoke_mock.mockImplementation((command) => {
      if (command === "list_all_tasks") {
        return Promise.resolve({ items: [], next_cursor: null });
      }
      if (command === "get_boards") {
        return Promise.resolve({ boards: store.boards, active_board_id: 7 });
      }
      if (
        command === "get_columns" ||
        command === "get_labels" ||
        command === "search_tasks"
      ) {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    await store.search_all_tasks({
      query: "",
      board_ids: null,
      column_id: null,
      status: "active",
      sort: "title",
      due: "all",
      due_start: null,
      due_end: null,
      archive_start: null,
      archive_end: null,
      now: Date.now(),
    });
    const initial_list_requests = invoke_mock.mock.calls.filter(
      ([command]) => command === "list_all_tasks",
    ).length;
    await expect(store.move_task_to_board("task_42", 8, 3)).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("move_task_to_board", {
      expectedBoardId: 7,
      taskId: 42,
      targetBoardId: 8,
      targetColumnId: 3,
    });
    expect(store.can_undo).toBe(false);
    expect(
      invoke_mock.mock.calls.filter(
        ([command]) => command === "list_all_tasks",
      ),
    ).toHaveLength(initial_list_requests + 1);
    await Promise.resolve();
    await Promise.resolve();
  });
});

describe("current board search", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps current results visible while refreshing the same query", async () => {
    vi.useFakeTimers();
    invoke_mock.mockReset();
    const store = create_store();
    invoke_mock.mockResolvedValueOnce([1]);
    store.search_tasks("plan");
    expect(store.search_pending).toBe(true);
    await vi.runAllTimersAsync();
    expect(store.search_task_ids).toEqual(new Set(["task_1"]));

    const refreshed = deferred<number[]>();
    invoke_mock.mockReturnValueOnce(refreshed.promise);
    store.search_tasks("plan");
    expect(store.search_pending).toBe(false);
    expect(store.search_task_ids).toEqual(new Set(["task_1"]));
    await vi.advanceTimersByTimeAsync(160);
    refreshed.resolve([1, 2]);
    await vi.runAllTimersAsync();
    expect(store.search_task_ids).toEqual(new Set(["task_1", "task_2"]));

    invoke_mock.mockResolvedValueOnce([]);
    store.search_tasks("planning");
    expect(store.search_pending).toBe(true);
    expect(store.search_task_ids).toBeNull();
    await vi.runAllTimersAsync();
  });
});

describe("board deletion summary", () => {
  it("loads the archived task count and treats failures as unknown", async () => {
    const store = create_store();
    invoke_mock.mockResolvedValueOnce(3);
    expect(await store.get_archived_count(7)).toBe(3);
    expect(invoke_mock).toHaveBeenLastCalledWith("get_board_archived_count", {
      boardId: 7,
    });
    invoke_mock.mockRejectedValueOnce({ code: "BOARD_NOT_FOUND" });
    expect(await store.get_archived_count(7)).toBeNull();
  });
});

describe("language persistence", () => {
  beforeEach(() => {
    vi.mocked(emit).mockReset();
    invoke_mock.mockReset();
    applyLanguagePreference("en");
  });
  afterEach(() => applyLanguagePreference("system"));

  it("applies a language only after settings are saved", async () => {
    const saved = deferred<void>();
    invoke_mock.mockReturnValue(saved.promise);
    const store = new BoardStore();
    const pending = store.set_language("zh-TW");
    expect(language.preference).toBe("en");
    expect(emit).not.toHaveBeenCalled();
    saved.resolve();
    expect(await pending).toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("set_language", {
      language: "zh-TW",
    });
    expect(language.preference).toBe("zh-TW");
    expect(emit).toHaveBeenCalledWith("cardbe:language-changed", "zh-TW");
  });
  it("keeps the previous language when persistence fails", async () => {
    invoke_mock.mockRejectedValue(new Error("disk unavailable"));
    expect(await new BoardStore().set_language("zh-TW")).toBe(false);
    expect(language.preference).toBe("en");
    expect(emit).not.toHaveBeenCalled();
  });
  it("keeps the saved language when broadcasting fails", async () => {
    invoke_mock.mockResolvedValue(undefined);
    vi.mocked(emit).mockRejectedValue(new Error("window unavailable"));
    expect(await new BoardStore().set_language("zh-TW")).toBe(true);
    expect(language.preference).toBe("zh-TW");
  });
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolve_promise, reject_promise) => {
    resolve = resolve_promise;
    reject = reject_promise;
  });
  return { promise, resolve, reject };
}

function create_store(column_count = 1) {
  const store = new BoardStore();
  store.iroh_enabled = true;
  store.boards = [
    {
      id: 7,
      name: "Test board",
      task_count: 0,
      shared_role: "owner",
      sync_status: "synced",
      sync_revision: 0,
    },
  ];
  store.active_board_id = 7;
  store.columns = Array.from({ length: column_count }, (_, index) => ({
    id: `column_${index + 1}`,
    name: `Column ${index + 1}`,
    color: "",
    sort_rules: [],
    tasks: [],
  }));
  return store;
}

describe("Iroh sync", () => {
  it("does not start Iroh or sync shared boards while disabled", () => {
    invoke_mock.mockReset();
    const store = create_store();
    store.iroh_enabled = false;
    store.boards[0].shared_role = "viewer";
    const sync = vi.spyOn(store, "sync_iroh_board");
    store.trigger_iroh_background_sync(true);
    expect(invoke_mock).not.toHaveBeenCalled();
    expect(sync).not.toHaveBeenCalled();
    store.dispose();
  });
  beforeEach(() => invoke_mock.mockReset());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it.each([
    [1, 0, true],
    [1, 1, false],
    [0, 1, false],
    [0, 0, true],
  ])(
    "reports owner sync results (%i synced, %i failed)",
    async (synced, failed, success) => {
      const store = create_store();
      store.boards[0].is_shared = true;
      vi.mocked(toast.success).mockClear();
      vi.mocked(toast.warning).mockClear();
      invoke_mock.mockResolvedValue({
        board: store.boards[0],
        content_changed: synced > 0,
        owner_sync: { synced_devices: synced, failed_devices: failed },
      });
      const fetch_columns = vi
        .spyOn(store, "get_columns")
        .mockImplementation(() => {});
      vi.spyOn(store, "update_labels").mockImplementation(() => {});
      vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
      store.can_undo = true;
      expect(await store.sync_iroh_board(7)).toBe(success);
      expect(Boolean(store.iroh_last_synced_at[7])).toBe(synced > 0);
      expect(fetch_columns).toHaveBeenCalledTimes(synced > 0 ? 1 : 0);
      expect(store.can_undo).toBe(synced === 0);
      if (failed > 0 || synced === 0) {
        expect(toast.warning).toHaveBeenCalledOnce();
        expect(toast.success).not.toHaveBeenCalled();
      } else {
        expect(toast.success).toHaveBeenCalledOnce();
      }
    },
  );

  it.each(
    ["SHARE_ACCESS_TERMINATED", "SHARE_INVITATION_DELETED"].flatMap((reason) =>
      ["synced", "pending", "conflict"].map((status) => ({ reason, status })),
    ),
  )(
    "removes $status boards for $reason and queues an explanation",
    async ({ reason, status }) => {
      const store = create_store();
      store.boards[0].shared_role = "editor";
      store.boards[0].sync_status = status;
      store.boards.push({ ...store.boards[0], id: 8, shared_role: "owner" });
      store.can_undo = true;
      vi.spyOn(store, "get_columns").mockImplementation(() => {});
      vi.spyOn(store, "update_labels").mockImplementation(() => {});
      vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
      invoke_mock.mockImplementation((command) => {
        if (
          command === "sync_iroh_board" ||
          command === "request_iroh_board_access"
        ) {
          return Promise.reject({ code: reason });
        }
        if (command === "delete_board") {
          return Promise.resolve(8);
        }
        return Promise.resolve();
      });
      expect(await store.sync_iroh_board(7, true, status === "conflict")).toBe(
        false,
      );
      expect(invoke_mock).toHaveBeenCalledWith("delete_board", { boardId: 7 });
      expect(store.boards.map((board) => board.id)).toEqual([8]);
      expect(store.active_board_id).toBe(8);
      expect(store.can_undo).toBe(false);
      expect(store.iroh_removal_notices).toEqual([
        { board_name: "Test board", reason, switched_to: "Test board" },
      ]);
    },
  );

  it.each([
    "INVITE_DISABLED",
    "SHARE_APPROVAL_REQUIRED",
    "SHARE_ACCESS_REVOKED",
    "INTERNAL_ERROR",
  ])(
    "retains local data for %s and never removes an owned board",
    async (code) => {
      const store = create_store();
      store.boards[0].shared_role = "viewer";
      expect(
        await store.remove_revoked_iroh_board(7, { code, device_id: "device" }),
      ).toBe(false);
      store.boards[0].shared_role = "owner";
      expect(
        await store.remove_revoked_iroh_board(7, {
          code: "SHARE_ACCESS_TERMINATED",
        }),
      ).toBe(false);
      expect(invoke_mock).not.toHaveBeenCalled();
      expect(store.boards).toHaveLength(1);
    },
  );

  it.each(["en", "zh-TW"] as const)(
    "replaces the last revoked board in %s",
    async (locale) => {
      const originalGetLocale = runtimeGetLocale;
      overwriteGetLocale(getLocale);
      applyLanguagePreference(locale);
      try {
        const store = create_store();
        store.boards[0].shared_role = "viewer";
        const replacement = { ...store.boards[0], id: 8, shared_role: "owner" };
        vi.spyOn(store, "get_columns").mockImplementation(() => {});
        vi.spyOn(store, "update_labels").mockImplementation(() => {});
        vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
        invoke_mock.mockImplementation((command) => {
          if (command === "create_board") {
            return Promise.resolve(replacement);
          }
          if (command === "delete_board") {
            return Promise.resolve(8);
          }
          return Promise.resolve();
        });
        expect(
          await store.remove_revoked_iroh_board(7, {
            code: "SHARE_ACCESS_TERMINATED",
          }),
        ).toBe(true);
        expect(invoke_mock).toHaveBeenCalledWith("create_board", {
          name: locale === "en" ? "My board" : "我的看板",
        });
        expect(store.boards).toEqual([replacement]);
        expect(store.active_board_id).toBe(8);
      } finally {
        applyLanguagePreference("en");
        overwriteGetLocale(originalGetLocale);
      }
    },
  );

  it("retries cleanup after a storage failure", async () => {
    const store = create_store();
    store.boards[0].shared_role = "viewer";
    store.boards.push({ ...store.boards[0], id: 8, shared_role: "owner" });
    const saved = [...store.boards];
    invoke_mock.mockImplementation((command) => {
      if (command === "sync_iroh_board") {
        return Promise.reject({ code: "SHARE_ACCESS_TERMINATED" });
      }
      if (command === "delete_board") {
        return Promise.reject({ code: "INTERNAL_ERROR" });
      }
      if (command === "get_boards") {
        return Promise.resolve({ boards: saved, active_board_id: 7 });
      }
      return Promise.resolve();
    });
    await store.sync_iroh_board(7, true);
    expect(store.boards).toHaveLength(2);
    expect(store.iroh_access_removed[7]).not.toBe(true);
    expect(store.iroh_removal_notices).toEqual([]);
  });

  it("queues each removed board once and does not claim a switch for inactive boards", async () => {
    const store = create_store();
    store.boards[0].shared_role = "viewer";
    store.boards.push({ ...store.boards[0], id: 8, name: "Second board" });
    store.boards.push({ ...store.boards[0], id: 9, shared_role: "owner" });
    store.active_board_id = 9;
    invoke_mock.mockResolvedValue(9);
    await store.remove_revoked_iroh_board(7, {
      code: "SHARE_INVITATION_DELETED",
    });
    await store.remove_revoked_iroh_board(8, {
      code: "SHARE_ACCESS_TERMINATED",
    });
    await store.remove_revoked_iroh_board(7, {
      code: "SHARE_INVITATION_DELETED",
    });
    expect(store.iroh_removal_notices).toEqual([
      {
        board_name: "Test board",
        reason: "SHARE_INVITATION_DELETED",
        switched_to: null,
      },
      {
        board_name: "Second board",
        reason: "SHARE_ACCESS_TERMINATED",
        switched_to: null,
      },
    ]);
    expect(invoke_mock).toHaveBeenCalledTimes(2);
  });

  it("keeps checking paused boards so later deletion can be detected", () => {
    invoke_mock.mockResolvedValue(undefined);
    vi.stubGlobal("document", { visibilityState: "visible" });
    const store = create_store();
    store.boards[0].shared_role = "viewer";
    store.iroh_access_removed[7] = true;
    const sync = vi.spyOn(store, "sync_iroh_board").mockResolvedValue(false);
    store.trigger_iroh_background_sync();
    expect(sync).toHaveBeenCalledWith(7, true, false);
  });

  it("restores revoked access without syncing an unresolved conflict", async () => {
    const store = create_store();
    store.boards[0].shared_role = "editor";
    store.boards[0].sync_status = "conflict";
    const saved = { ...store.boards[0] };
    invoke_mock.mockImplementation((command) => {
      if (command === "get_boards") {
        return Promise.resolve({ boards: [saved], active_board_id: 7 });
      }
      if (command === "request_iroh_board_access") {
        return Promise.reject({ code: "SHARE_ACCESS_REVOKED" });
      }
      return Promise.resolve();
    });
    await store.sync_iroh_board(7, true, true);
    expect(store.iroh_access_removed[7]).toBe(true);

    // The approval response refreshes permissions while retaining both versions.
    store.apply_iroh_conflict_refresh({ ...saved, shared_role: "viewer" });
    invoke_mock.mockClear();
    vi.mocked(toast.error).mockClear();
    await store.restore_iroh_access(7);

    expect(invoke_mock).not.toHaveBeenCalled();
    expect(store.iroh_access_removed[7]).toBe(false);
    expect(store.iroh_access_error[7]).toBeUndefined();
    expect(store.iroh_sync_error[7]).toBeUndefined();
    expect(store.boards[0].sync_status).toBe("conflict");
    expect(store.boards[0].shared_role).toBe("viewer");
    expect(toast.success).toHaveBeenCalledWith(
      "Access restored. Resolve the conflict before syncing.",
    );
    expect(toast.error).not.toHaveBeenCalled();
  });

  it("syncs after access is restored when there is no conflict", async () => {
    const store = create_store();
    const sync = vi.spyOn(store, "sync_iroh_board").mockResolvedValue(true);
    await store.restore_iroh_access(7);
    expect(sync).toHaveBeenCalledWith(7, true);
    expect(toast.success).toHaveBeenCalledWith("Access restored");
  });

  it("lets a pending board sync after checking two conflicting boards", async () => {
    vi.stubGlobal("document", { visibilityState: "visible" });
    vi.spyOn(Date, "now").mockReturnValue(10_000);
    const store = create_store();
    store.active_board_id = null;
    store.boards = [7, 8, 9].map((id) => ({
      ...store.boards[0],
      id,
      shared_role: "editor" as const,
      sync_status: id === 9 ? "pending" : "conflict",
    }));
    store.iroh_last_synced_at = { 7: 100, 8: 100, 9: 200 };
    invoke_mock.mockImplementation((command) => {
      if (command === "request_iroh_board_access") {
        return Promise.resolve([true, "editor"]);
      }
      if (command === "sync_iroh_board") {
        return Promise.resolve({
          board: {
            ...store.boards.find((board) => board.id === 9),
            sync_status: "synced",
          },
          content_changed: false,
        });
      }
      return Promise.resolve();
    });
    store.trigger_iroh_background_sync();
    expect(
      invoke_mock.mock.calls
        .filter(([command]) => command === "request_iroh_board_access")
        .map(([, args]) => args),
    ).toEqual([
      { boardId: 7, requestApproval: false },
      { boardId: 8, requestApproval: false },
    ]);
    expect(invoke_mock).not.toHaveBeenCalledWith(
      "sync_iroh_board",
      expect.anything(),
    );
    await Promise.resolve();
    expect(store.iroh_last_synced_at).toEqual({ 7: 100, 8: 100, 9: 200 });
    store.trigger_iroh_background_sync();
    await vi.waitFor(() =>
      expect(invoke_mock).toHaveBeenCalledWith("sync_iroh_board", {
        boardId: 9,
      }),
    );
    expect(
      store.boards
        .filter((board) => board.sync_status === "conflict")
        .map((board) => board.id),
    ).toEqual([7, 8]);
  });

  it("retries a failed viewer immediately after connection settings change", async () => {
    vi.stubGlobal("document", { visibilityState: "visible" });
    const store = create_store();
    store.boards[0].shared_role = "viewer";
    store.active_board_id = null;
    store.iroh_last_synced_at[7] = Date.now();
    const synced = { ...store.boards[0], sync_status: "synced" as const };
    let attempts = 0;
    invoke_mock.mockImplementation((command) => {
      if (command === "sync_iroh_board") {
        attempts += 1;
        return attempts === 1
          ? Promise.reject("Owner unreachable")
          : Promise.resolve({ board: synced, content_changed: false });
      }
      if (command === "get_boards") {
        return Promise.resolve({ boards: store.boards, active_board_id: null });
      }
      return Promise.resolve();
    });
    await store.sync_iroh_board(7, true);
    expect(store.iroh_sync_error[7]).toEqual({ code: "INTERNAL_ERROR" });
    store.trigger_iroh_background_sync();
    expect(attempts).toBe(1);
    store.reconnect_iroh_boards();
    await vi.waitFor(() => expect(attempts).toBe(2));
    await vi.waitFor(() => expect(store.iroh_sync_error[7]).toBeUndefined());
    expect(store.boards[0].sync_status).toBe("synced");
  });

  it("reconnects after an old in-flight sync fails without applying its retry delay", async () => {
    vi.stubGlobal("document", { visibilityState: "visible" });
    const store = create_store();
    store.boards[0].shared_role = "editor";
    store.can_undo = true;
    const interrupted = deferred<unknown>();
    let attempts = 0;
    invoke_mock.mockImplementation((command) => {
      if (command === "sync_iroh_board") {
        attempts += 1;
        return attempts === 1
          ? interrupted.promise
          : Promise.resolve({
              board: { ...store.boards[0], sync_status: "synced" },
              content_changed: false,
            });
      }
      return Promise.resolve();
    });
    const original = store.sync_iroh_board(7, true);
    store.reconnect_iroh_boards();
    expect(attempts).toBe(1);
    interrupted.reject("Old endpoint was closed");
    await original;
    await vi.waitFor(() => expect(attempts).toBe(2));
    expect(store.iroh_sync_error[7]).toBeUndefined();
    expect(store.can_undo).toBe(true);
  });

  it("keeps pending edits marked pending if reconnecting also fails", async () => {
    vi.stubGlobal("document", { visibilityState: "visible" });
    const store = create_store();
    store.boards[0].shared_role = "editor";
    store.boards[0].sync_status = "pending";
    const saved = { ...store.boards[0] };
    const interrupted = deferred<unknown>();
    let attempts = 0;
    invoke_mock.mockImplementation((command) => {
      if (command === "sync_iroh_board") {
        attempts += 1;
        return attempts === 1
          ? interrupted.promise
          : Promise.reject("Owner still unreachable");
      }
      if (command === "get_boards") {
        return Promise.resolve({ boards: [saved], active_board_id: 7 });
      }
      return Promise.resolve();
    });
    const original = store.sync_iroh_board(7, true);
    store.reconnect_iroh_boards();
    interrupted.reject("Old endpoint was closed");
    await original;
    await vi.waitFor(() =>
      expect(store.iroh_sync_error[7]).toEqual({ code: "INTERNAL_ERROR" }),
    );
    expect(store.boards[0].sync_status).toBe("pending");
  });

  it("keeps conflict data and sync timestamps unchanged when access is still approved", async () => {
    const store = create_store();
    store.boards[0].shared_role = "editor";
    store.boards[0].sync_status = "conflict";
    store.iroh_last_synced_at[7] = 1000;
    store.can_undo = true;
    invoke_mock.mockResolvedValue([true, "device"]);

    expect(await store.sync_iroh_board(7, true, true)).toBe(true);
    expect(store.boards[0].sync_status).toBe("conflict");
    expect(store.iroh_last_synced_at[7]).toBe(1000);
    expect(store.can_undo).toBe(true);
    expect(invoke_mock).toHaveBeenCalledTimes(1);
    expect(invoke_mock).toHaveBeenCalledWith("request_iroh_board_access", {
      boardId: 7,
      requestApproval: false,
    });
  });

  it("updates restored editor permission while preserving conflict content and sync time", async () => {
    const store = create_store();
    store.boards[0].shared_role = "viewer";
    store.boards[0].sync_status = "conflict";
    store.iroh_last_synced_at[7] = 1000;
    const columns = store.columns;
    invoke_mock.mockResolvedValue([
      true,
      "device",
      { ...store.boards[0], shared_role: "editor" },
    ]);

    expect(await store.sync_iroh_board(7, true, true)).toBe(true);
    expect(store.boards[0].shared_role).toBe("editor");
    expect(store.boards[0].sync_status).toBe("conflict");
    expect(store.columns).toBe(columns);
    expect(store.iroh_last_synced_at[7]).toBe(1000);
  });

  it.each([false, true])(
    "ignores a delayed conflict refresh after resolution (keep local: %s)",
    async (keep_local) => {
      const store = create_store();
      store.active_board_id = null;
      store.boards[0].shared_role = "editor";
      store.boards[0].sync_status = "conflict";
      const stale = { ...store.boards[0], shared_role: "viewer" as const };
      const resolved = {
        ...store.boards[0],
        sync_status: keep_local ? "pending" : "synced",
      };
      const response = deferred<[boolean, string, typeof stale]>();
      invoke_mock.mockImplementation((command) => {
        if (command === "request_iroh_board_access") {
          return response.promise;
        }
        if (command === "get_boards") {
          return Promise.resolve({ boards: [resolved], active_board_id: null });
        }
        return Promise.resolve();
      });
      vi.spyOn(store, "trigger_iroh_background_sync").mockImplementation(
        () => {},
      );

      const checking = store.sync_iroh_board(7, true, true);
      expect(await store.resolve_iroh_conflict(7, keep_local)).toBe(true);
      response.resolve([true, "device", stale]);
      await checking;
      expect(store.boards[0]).toEqual(resolved);

      // The share dialog uses the same guard for its access responses.
      store.apply_iroh_conflict_refresh(stale);
      expect(store.boards[0]).toEqual(resolved);
    },
  );

  it.each(["viewer", "conflict", "pending"])(
    "reports revoked access during silent background checks for %s boards",
    async (state) => {
      vi.stubGlobal("document", { visibilityState: "visible" });
      vi.spyOn(Date, "now").mockReturnValue(1000);
      vi.mocked(toast.error).mockClear();
      const store = create_store();
      store.boards[0].shared_role = state === "viewer" ? "viewer" : "editor";
      store.boards[0].sync_status = state === "viewer" ? "synced" : state;
      store.iroh_last_synced_at[7] = 1000;
      const saved = { ...store.boards[0] };
      invoke_mock.mockImplementation((command) => {
        if (command === "get_boards") {
          return Promise.resolve({ boards: [saved], active_board_id: 7 });
        }
        if (
          command === "request_iroh_board_access" ||
          command === "sync_iroh_board"
        ) {
          return Promise.reject({ code: "SHARE_ACCESS_REVOKED" });
        }
        return Promise.resolve();
      });

      store.trigger_iroh_background_sync();

      await vi.waitFor(() => expect(store.iroh_access_removed[7]).toBe(true));
      await vi.waitFor(() =>
        expect(store.boards[0].sync_status).toBe(
          state === "conflict" ? "conflict" : "error",
        ),
      );
      expect(store.iroh_access_error[7]).toEqual({
        code: "SHARE_ACCESS_REVOKED",
      });
      expect(toast.error).not.toHaveBeenCalled();
      if (state !== "pending") {
        expect(invoke_mock).toHaveBeenCalledWith("request_iroh_board_access", {
          boardId: 7,
          requestApproval: false,
        });
      }
    },
  );

  it("rotates through boards with two slots even when the system reports offline", async () => {
    vi.stubGlobal("navigator", { onLine: false });
    vi.stubGlobal("document", { visibilityState: "visible" });
    const clock = vi.spyOn(Date, "now").mockReturnValue(1000);
    const store = create_store();
    store.active_board_id = null;
    store.boards = [1, 2, 3].map((id) => ({
      ...store.boards[0],
      id,
      shared_role: "editor",
    }));
    const responses = [deferred<unknown>(), deferred<unknown>()];
    const started: number[] = [];
    invoke_mock.mockImplementation((command, args) => {
      if (command !== "sync_iroh_board") {
        return Promise.resolve();
      }
      const id = (args as { boardId: number }).boardId;
      started.push(id);
      return (
        responses[id - 1]?.promise ??
        Promise.resolve({
          board: store.boards.find((board) => board.id === id),
          content_changed: false,
        })
      );
    });

    store.trigger_iroh_background_sync();
    store.trigger_iroh_background_sync();
    expect(started).toEqual([1, 2]);
    responses.forEach((response, index) =>
      response.resolve({
        board: { ...store.boards[index], sync_status: "synced" },
        content_changed: false,
      }),
    );
    await vi.waitFor(() => expect(store.iroh_last_synced_at[2]).toBe(1000));
    clock.mockReturnValue(2000);
    store.trigger_iroh_background_sync();
    expect(started.slice(2)).toEqual([3, 1]);
    await vi.waitFor(() => expect(store.iroh_last_synced_at[3]).toBe(2000));
  });

  it.each([false, true])(
    "only clears local Undo and reloads for changed content (%s)",
    async (content_changed) => {
      const store = create_store();
      store.boards[0].shared_role = "editor";
      store.can_undo = true;
      const reload = vi
        .spyOn(store, "get_columns")
        .mockImplementation(() => {});
      vi.spyOn(store, "update_labels").mockImplementation(() => {});
      vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
      invoke_mock.mockResolvedValue({
        board: { ...store.boards[0] },
        content_changed,
      });

      expect(await store.sync_iroh_board(7, true)).toBe(true);
      expect(store.can_undo).toBe(!content_changed);
      expect(reload).toHaveBeenCalledTimes(content_changed ? 1 : 0);
    },
  );
});

describe("Undo scope and shared Task Explorer updates", () => {
  beforeEach(() => {
    invoke_mock.mockReset();
    vi.mocked(toast.success).mockClear();
  });

  it("rejects a task edit after its board becomes read-only without changing local data", async () => {
    const store = create_store();
    const task = create_task("task_1", "Original");
    store.columns[0].tasks = [task];
    store.boards[0].shared_role = "editor";
    expect(store.can_edit).toBe(true);
    const edited_task = { ...task, title: "Edited" };
    store.boards[0].shared_role = "viewer";
    expect(await store.update_task(task.id, edited_task)).toBe(false);
    expect(store.columns[0].tasks[0].title).toBe("Original");
    expect(invoke_mock).not.toHaveBeenCalled();
  });

  it.each(["editor", "viewer", "shared owner"])(
    "allows Undo for local edits only when a %s board is editable",
    async (role) => {
      const store = create_store();
      if (role === "shared owner") {
        store.boards[0].is_shared = true;
      } else {
        store.boards[0].shared_role = role as "editor" | "viewer";
      }
      invoke_mock.mockImplementation((command) =>
        Promise.resolve(
          command === "add_task" ? 42 : command === "undo" ? false : [],
        ),
      );
      await store.add_new_task("column_1", create_task("", "Task"));
      store.can_undo = true;
      const editable = role !== "viewer";
      expect(store.can_undo).toBe(editable);
      expect(await store.undo()).toBe(editable);
      expect(
        invoke_mock.mock.calls.some(([command]) => command === "undo"),
      ).toBe(editable);
      expect(
        vi
          .mocked(toast.success)
          .mock.calls.some(([, options]) => options?.action),
      ).toBe(editable);
    },
  );

  it("redoes undone changes until another change is made", async () => {
    const store = create_store();
    let redo_left = true;
    invoke_mock.mockImplementation((command) =>
      Promise.resolve(
        command === "undo" ? true : command === "redo" ? redo_left : [],
      ),
    );
    expect(await store.redo()).toBe(false);
    store.can_undo = true;
    expect(await store.undo()).toBe(true);
    expect(store.can_redo).toBe(true);
    redo_left = false;
    expect(await store.redo()).toBe(true);
    expect(
      invoke_mock.mock.calls.find(([command]) => command === "redo")?.[1],
    ).toEqual({ expectedBoardId: store.active_board_id });
    expect(store.can_undo).toBe(true);
    expect(store.can_redo).toBe(false);

    expect(await store.undo()).toBe(true);
    expect(store.can_redo).toBe(true);
    // A new local change, such as adding a task.
    store.can_undo = true;
    expect(store.can_redo).toBe(false);
  });

  it.each([false, true])(
    "settings preserve existing Undo availability (%s)",
    async (has_history) => {
      const store = create_store();
      store.can_undo = has_history;
      vi.mocked(isPermissionGranted).mockResolvedValue(true);
      invoke_mock.mockResolvedValue(undefined);
      vi.useFakeTimers();
      await store.set_notify_enabled(true);
      expect(store.notify_enabled).toBe(true);
      expect(store.can_undo).toBe(has_history);
      vi.clearAllTimers();
      vi.useRealTimers();
    },
  );

  it("restores calendar date changes when saving the task fails", async () => {
    const store = create_store();
    vi.mocked(toast.error).mockClear();
    const task = create_task(
      "task_1",
      "Span",
      "",
      "",
      new Date(2026, 9, 19, 9),
      new Date(2026, 9, 22, 18),
    );
    store.columns[0].tasks = [task];
    let reject_update: (error: unknown) => void = () => {};
    invoke_mock.mockImplementation((command) =>
      command === "update_task"
        ? new Promise((_, reject) => {
            reject_update = reject;
          })
        : Promise.resolve([]),
    );

    const update = store.update_task(task.id, {
      ...task,
      start_time: new Date(2026, 9, 21, 9),
      due_time: new Date(2026, 9, 24, 18),
    });
    expect(store.columns[0].tasks[0].start_time).toEqual(
      new Date(2026, 9, 21, 9),
    );
    await vi.waitFor(() =>
      expect(
        invoke_mock.mock.calls.some(([command]) => command === "update_task"),
      ).toBe(true),
    );
    reject_update({ code: "INTERNAL_ERROR" });

    expect(await update).toBe(false);
    expect(store.columns[0].tasks[0].start_time).toEqual(
      new Date(2026, 9, 19, 9),
    );
    expect(store.columns[0].tasks[0].due_time).toEqual(
      new Date(2026, 9, 22, 18),
    );
    expect(toast.error).toHaveBeenCalled();
  });

  it("persists task expansion and restores it when saving fails", async () => {
    const store = create_store();
    store.can_undo = false;
    vi.mocked(toast.error).mockClear();
    invoke_mock.mockResolvedValue(undefined);
    store.task_expansion.toggle(7, "1");
    store.task_expansion.toggle(9, "1");
    await store.set_task_expand_mode(false);
    expect(store.task_expansion.is_expanded(7, "1")).toBe(false);
    expect(store.task_expansion.is_expanded(9, "1")).toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("set_task_expand_mode", {
      boardId: 7,
      expanded: false,
    });
    expect(store.task_expand_mode).toBe(false);
    expect(store.can_undo).toBe(false);

    invoke_mock.mockRejectedValueOnce({ code: "INTERNAL_ERROR" });
    await store.set_task_expand_mode(true);
    expect(store.task_expand_mode).toBe(false);
    expect(toast.error).toHaveBeenCalled();

    // Other boards keep their own mode.
    store.active_board_id = 9;
    expect(store.task_expand_mode).toBe(true);
    store.active_board_id = 7;
    expect(store.task_expand_mode).toBe(false);
  });

  it("applies the default task expansion to every board", async () => {
    const store = create_store();
    vi.mocked(toast.error).mockClear();
    invoke_mock.mockResolvedValue(undefined);
    await store.set_task_expand_mode(false);
    store.task_expansion.toggle(9, "1");
    await store.set_default_task_expand_mode(true);
    expect(invoke_mock).toHaveBeenCalledWith("set_default_task_expand_mode", {
      expanded: true,
    });
    expect(store.task_expand_mode).toBe(true);
    expect(store.task_expansion.is_expanded(9, "1")).toBe(false);

    invoke_mock.mockRejectedValueOnce({ code: "INTERNAL_ERROR" });
    await store.set_default_task_expand_mode(false);
    expect(store.default_task_expand_mode).toBe(true);
    expect(store.task_expand_mode).toBe(true);
    expect(toast.error).toHaveBeenCalled();
  });

  it("persists the calendar layout and restores it when saving fails", async () => {
    const store = create_store();
    store.can_undo = false;
    vi.mocked(toast.error).mockClear();
    invoke_mock.mockResolvedValue(undefined);
    await store.set_calendar_expand_all(true);
    expect(invoke_mock).toHaveBeenCalledWith("set_calendar_expand_all", {
      boardId: 7,
      expanded: true,
    });
    expect(store.calendar_expand_all).toBe(true);
    expect(store.can_undo).toBe(false);

    invoke_mock.mockRejectedValueOnce({ code: "INTERNAL_ERROR" });
    await store.set_calendar_expand_all(false);
    expect(store.calendar_expand_all).toBe(true);
    expect(toast.error).toHaveBeenCalled();

    // Other boards keep their own layout.
    store.active_board_id = 9;
    expect(store.calendar_expand_all).toBe(false);
    store.active_board_id = 7;
    expect(store.calendar_expand_all).toBe(true);
  });

  it("saves the reminder time and restores it when saving fails", async () => {
    const store = create_store();
    vi.mocked(toast.error).mockClear();
    invoke_mock.mockResolvedValue(undefined);
    await store.set_notify_reminder(15);
    expect(invoke_mock).toHaveBeenCalledWith("set_notify_reminder", {
      minutes: 15,
    });
    expect(store.notify_reminder_minutes).toBe(15);

    invoke_mock.mockRejectedValueOnce({ code: "INVALID_ARGUMENT" });
    await store.set_notify_reminder(60);
    expect(store.notify_reminder_minutes).toBe(15);
    expect(toast.error).toHaveBeenCalled();
  });

  it("mutes column notifications per board and restores them when saving fails", async () => {
    const store = create_store();
    vi.mocked(toast.error).mockClear();
    invoke_mock.mockResolvedValue(undefined);
    await store.set_column_notify_muted("column_3", true);
    expect(invoke_mock).toHaveBeenCalledWith("set_column_notify_muted", {
      boardId: 7,
      columnId: 3,
      muted: true,
    });
    expect(store.is_column_notify_muted("column_3")).toBe(true);

    invoke_mock.mockRejectedValueOnce({ code: "INTERNAL_ERROR" });
    await store.set_column_notify_muted("column_3", false);
    expect(store.is_column_notify_muted("column_3")).toBe(true);
    expect(toast.error).toHaveBeenCalled();

    // Other boards keep their own columns.
    store.active_board_id = 9;
    expect(store.is_column_notify_muted("column_3")).toBe(false);
    store.active_board_id = 7;
    await store.set_column_notify_muted("column_3", false);
    expect(store.is_column_notify_muted("column_3")).toBe(false);
  });

  it.each(["sync", "push"])(
    "refreshes Explorer after an inactive shared board %s",
    async (event) => {
      const store = create_store();
      const shared = {
        ...store.boards[0],
        id: 9,
        shared_role: "editor" as const,
        is_shared: true,
      };
      store.boards.push(shared);
      const filter: TaskExplorerQuery = {
        query: "",
        board_ids: null,
        column_id: null,
        status: "active",
        sort: "due",
        due: "all",
        due_start: null,
        due_end: null,
        archive_start: null,
        archive_end: null,
        now: 0,
      };
      invoke_mock.mockImplementation((command) => {
        if (command === "list_all_tasks") {
          return Promise.resolve({ items: [], next_cursor: null });
        }
        if (command === "get_boards") {
          return Promise.resolve({ boards: store.boards, active_board_id: 7 });
        }
        return Promise.resolve({ board: shared, content_changed: true });
      });
      await store.search_all_tasks(filter);
      if (event === "sync") {
        await store.sync_iroh_board(9, true);
      } else {
        await store.handle_iroh_remote_push(9, 1);
      }
      expect(
        invoke_mock.mock.calls.filter(
          ([command]) => command === "list_all_tasks",
        ),
      ).toHaveLength(2);
      expect(store.active_board_id).toBe(7);
    },
  );
});

describe("initial board loading", () => {
  it("keeps a switched board unready until its own columns arrive", async () => {
    invoke_mock.mockReset();
    const columns = deferred<[]>();
    invoke_mock.mockImplementation((command) =>
      command === "get_columns" ? columns.promise : Promise.resolve([]),
    );
    const store = create_store();
    store.column_fetch_finish = true;
    store.column_fetch_error = true;
    await store.switch_board(9);
    expect(store.active_board_id).toBe(9);
    expect(store.column_fetch_finish).toBe(false);
    expect(store.column_fetch_error).toBe(false);
    columns.resolve([]);
    await vi.waitFor(() => expect(store.column_fetch_finish).toBe(true));
    store.dispose();
  });
  beforeEach(() => {
    invoke_mock.mockReset();
  });

  it("loads columns without waiting for an earlier background mission", async () => {
    const labels = deferred<string[]>();
    invoke_mock.mockImplementation((command) => {
      if (command === "get_labels") {
        return labels.promise;
      }
      if (command === "get_columns") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    store.update_labels();
    store.get_columns();

    expect(invoke_mock).toHaveBeenCalledWith("get_columns", {
      expectedBoardId: 7,
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(store.column_fetch_finish).toBe(true);
    expect(store.column_fetch_error).toBe(false);
    labels.resolve([]);
    await Promise.resolve();
  });

  it("leaves the initial loading state when the column IPC request times out", async () => {
    vi.useFakeTimers();
    try {
      const columns = deferred<never>();
      invoke_mock.mockImplementation((command) => {
        if (command === "get_columns") {
          return columns.promise;
        }
        return Promise.resolve();
      });
      const store = create_store();

      store.get_columns();
      await vi.advanceTimersByTimeAsync(15_000);

      expect(store.column_fetch_finish).toBe(true);
      expect(store.column_fetch_error).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("pending task operations", () => {
  beforeEach(() => {
    invoke_mock.mockReset();
  });

  it("waits for creation before deleting a new task", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return add_task.promise;
      }
      if (command === "delete_task") {
        return Promise.resolve();
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const pending_task_id = store.columns[0].tasks[0].id;
    const deletion = store.delete_task(pending_task_id);

    expect(store.columns[0].tasks).toHaveLength(0);
    add_task.resolve(42);

    await expect(creation).resolves.toBe(true);
    await expect(deletion).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("delete_task", {
      taskId: 42,
      expectedBoardId: 7,
    });
  });

  it("moves selected tasks in the requested order as one change", async () => {
    invoke_mock.mockResolvedValue(undefined);
    const store = create_store(2);
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
    ];
    store.columns[1].tasks = [
      create_task("task_3", "Three"),
      create_task("task_4", "Four"),
    ];

    const move = store.move_tasks(["task_3", "task_1"], "column_2", "task_4");

    expect(
      store.columns.map((column) => column.tasks.map((t) => t.id)),
    ).toEqual([["task_2"], ["task_3", "task_1", "task_4"]]);
    await expect(move).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("move_tasks", {
      taskIds: [3, 1],
      toColumnId: 2,
      beforeTaskId: 4,
      expectedBoardId: 7,
    });
    expect(invoke_mock).not.toHaveBeenCalledWith(
      "move_task",
      expect.anything(),
    );
  });

  it("pins selected tasks with one command", async () => {
    invoke_mock.mockResolvedValue(undefined);
    const store = create_store();
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
      create_task("task_3", "Three"),
    ];

    const pin = store.set_tasks_pinned(["task_3", "task_2"], true);

    expect(store.columns[0].tasks.map((task) => task.id)).toEqual([
      "task_2",
      "task_3",
      "task_1",
    ]);
    await expect(pin).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("set_tasks_pinned", {
      taskIds: [3, 2],
      pinned: true,
      expectedBoardId: 7,
    });
  });

  it("sets and clears the priority of selected tasks with one command", async () => {
    invoke_mock.mockResolvedValue(undefined);
    const store = create_store();
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
    ];

    await expect(
      store.set_tasks_priority(["task_2", "task_1"], "high"),
    ).resolves.toBe(true);
    expect(store.columns[0].tasks.map((task) => task.priority)).toEqual([
      "high",
      "high",
    ]);
    expect(invoke_mock).toHaveBeenCalledWith("set_tasks_priority", {
      taskIds: [2, 1],
      priority: "high",
      expectedBoardId: 7,
    });

    await expect(store.set_tasks_priority(["task_1"], undefined)).resolves.toBe(
      true,
    );
    expect("priority" in store.columns[0].tasks[0]).toBe(false);
    expect(store.columns[0].tasks[1].priority).toBe("high");
    expect(invoke_mock).toHaveBeenLastCalledWith("set_tasks_priority", {
      taskIds: [1],
      priority: null,
      expectedBoardId: 7,
    });
  });

  it("completes tasks into the done column, or archives without one", async () => {
    // Keep the optimistic board; the refreshed columns come from the backend.
    invoke_mock.mockImplementation((command: string) =>
      command === "get_columns"
        ? new Promise(() => {})
        : Promise.resolve(undefined),
    );
    const store = create_store(2);
    store.columns[1].done = true;
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
    ];
    store.columns[1].tasks = [create_task("task_3", "Done")];

    const completed = store.complete_tasks(["task_2", "task_3"]);
    expect(store.columns[0].tasks.map((task) => task.id)).toEqual(["task_1"]);
    expect(store.columns[1].tasks.map((task) => task.id)).toEqual([
      "task_3",
      "task_2",
    ]);
    await expect(completed).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("complete_tasks", {
      taskIds: [2, 3],
      expectedBoardId: 7,
    });

    store.columns[1].done = false;
    void store.complete_tasks(["task_1"]);
    expect(store.columns[0].tasks).toEqual([]);
  });

  it("reopens only done tasks in an open column", async () => {
    invoke_mock.mockImplementation((command: string) =>
      command === "get_columns"
        ? new Promise(() => {})
        : Promise.resolve(undefined),
    );
    const store = create_store(2);
    store.columns[1].done = true;
    store.columns[0].tasks = [create_task("task_1", "Open")];
    store.columns[1].tasks = [create_task("task_2", "Done")];

    await expect(store.reopen_tasks(["task_2"], "column_2")).resolves.toBe(
      false,
    );
    const reopened = store.reopen_tasks(["task_1", "task_2"], "column_1");
    expect(store.columns[0].tasks.map((task) => task.id)).toEqual([
      "task_1",
      "task_2",
    ]);
    await expect(reopened).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("reopen_tasks", {
      taskIds: [1, 2],
      columnId: 1,
      expectedBoardId: 7,
    });
  });

  it("moves selected tasks to another board with one command", async () => {
    invoke_mock.mockResolvedValue([]);
    const store = create_store();
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
    ];
    vi.spyOn(store, "get_boards").mockResolvedValue(undefined);

    await expect(
      store.move_tasks_to_board(["task_1", "task_2"], 8, 3),
    ).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("move_tasks_to_board", {
      expectedBoardId: 7,
      taskIds: [1, 2],
      targetBoardId: 8,
      targetColumnId: 3,
    });
    expect(store.can_undo).toBe(false);
  });

  it("waits for pending creations before deleting selected tasks", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) =>
      command === "add_task" ? add_task.promise : Promise.resolve([]),
    );
    const store = create_store();
    store.columns[0].tasks = [create_task("task_1", "Existing")];
    const creation = store.add_new_task("column_1", create_task("", "New"));
    const pending_task_id = store.columns[0].tasks[1].id;

    const deletion = store.delete_tasks(["task_1", pending_task_id]);

    expect(store.columns[0].tasks).toHaveLength(0);
    add_task.resolve(42);
    await expect(creation).resolves.toBe(true);
    await expect(deletion).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("delete_tasks", {
      taskIds: [1, 42],
      expectedBoardId: 7,
    });
  });

  it("reloads the board when archiving selected tasks fails", async () => {
    invoke_mock.mockImplementation((command) =>
      command === "archive_tasks"
        ? Promise.reject({ code: "TASK_NOT_FOUND" })
        : Promise.resolve([]),
    );
    const store = create_store();
    store.columns[0].tasks = [
      create_task("task_1", "One"),
      create_task("task_2", "Two"),
    ];
    const get_columns = vi
      .spyOn(store, "get_columns")
      .mockImplementation(() => {});

    await expect(store.archive_tasks(["task_1", "task_2"])).resolves.toBe(
      false,
    );
    expect(store.columns[0].tasks).toHaveLength(0);
    expect(get_columns).toHaveBeenCalled();
    expect(toast.error).toHaveBeenCalled();
  });

  it("does not restore a pending task when its creation fails", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return add_task.promise;
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const deletion = store.delete_task(store.columns[0].tasks[0].id);
    add_task.reject(new Error("creation failed"));

    await expect(creation).resolves.toBe(false);
    await expect(deletion).resolves.toBe(false);
    expect(store.columns[0].tasks).toHaveLength(0);
    expect(invoke_mock).not.toHaveBeenCalledWith(
      "delete_task",
      expect.anything(),
    );
  });

  it("uses the persisted source ID when duplicating a new task", async () => {
    const first_add = deferred<number>();
    let add_count = 0;
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        add_count += 1;
        return add_count === 1 ? first_add.promise : Promise.resolve(43);
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const pending_task_id = store.columns[0].tasks[0].id;
    const duplication = store.duplicate_task(pending_task_id);
    first_add.resolve(42);

    await expect(creation).resolves.toBe(true);
    await expect(duplication).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith(
      "add_task",
      expect.objectContaining({ afterTaskId: 42 }),
    );
    expect(store.columns[0].tasks.map((task) => task.id)).toEqual([
      "task_42",
      "task_43",
    ]);
  });

  it("keeps the temporary ID as the render key after creation", async () => {
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return Promise.resolve(42);
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const pending_task_id = store.columns[0].tasks[0].id;

    await expect(creation).resolves.toBe(true);
    expect(store.columns[0].tasks[0].id).toBe("task_42");
    expect(store.task_render_key("task_42")).toBe(pending_task_id);
    expect(store.task_render_key("task_7")).toBe("task_7");
  });

  it("waits for creation before updating a new task", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return add_task.promise;
      }
      if (command === "update_task") {
        return Promise.resolve();
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const pending_task = store.columns[0].tasks[0];
    const update = store.update_task(pending_task.id, {
      ...pending_task,
      title: "Edited task",
    });
    add_task.resolve(42);

    await expect(creation).resolves.toBe(true);
    await expect(update).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("update_task", {
      taskId: 42,
      task: expect.objectContaining({ id: 42, title: "Edited task" }),
      expectedBoardId: 7,
    });
  });

  it("resolves a temporary ID retained by a dialog after creation", async () => {
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return Promise.resolve(42);
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      if (command === "save_task_template") {
        return Promise.resolve({
          id: 7,
          name: "Template",
          task: {
            id: 42,
            title: "New task",
            description: "",
            color: "",
            start_time: 0,
            due_time: undefined,
            labels: [],
            items: [],
            recurrence: undefined,
          },
        });
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const retained_pending_id = store.columns[0].tasks[0].id;
    await expect(creation).resolves.toBe(true);

    await expect(
      store.save_task_template(retained_pending_id, "Template"),
    ).resolves.toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("save_task_template", {
      taskId: 42,
      name: "Template",
      expectedBoardId: 7,
    });
  });

  it("finds a created task through its retained temporary ID", async () => {
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return Promise.resolve(42);
      }
      if (command === "delete_task") {
        return Promise.resolve();
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const retained_pending_id = store.columns[0].tasks[0].id;
    await expect(creation).resolves.toBe(true);

    await expect(store.delete_task(retained_pending_id)).resolves.toBe(true);
    expect(store.columns[0].tasks).toHaveLength(0);
    expect(invoke_mock).toHaveBeenCalledWith("delete_task", {
      taskId: 42,
      expectedBoardId: 7,
    });
  });

  it("persists a drag made before creation finishes", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return add_task.promise;
      }
      if (command === "move_task") {
        return Promise.resolve();
      }
      if (command === "get_labels") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store(2);

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    expect(store.move_task(0, 0, 1, null)).toBe(true);
    add_task.resolve(42);

    await expect(creation).resolves.toBe(true);
    await new Promise((resolve) => setTimeout(resolve, 350));
    expect(invoke_mock).toHaveBeenCalledWith("move_task", {
      taskId: 42,
      toColumnId: 2,
      beforeTaskId: null,
      expectedBoardId: 7,
    });
  });

  it("keeps a queued pending-task deletion bound to the board where it began", async () => {
    const add_task = deferred<number>();
    invoke_mock.mockImplementation((command) => {
      if (command === "add_task") {
        return add_task.promise;
      }
      if (command === "delete_task" || command === "switch_board") {
        return Promise.resolve();
      }
      if (command === "get_columns") {
        return Promise.resolve([]);
      }
      if (command === "get_labels" || command === "get_task_templates") {
        return Promise.resolve([]);
      }
      return Promise.resolve();
    });
    const store = create_store();
    store.boards = [
      {
        id: 7,
        name: "First",
        task_count: 0,
        shared_role: "owner",
        sync_status: "synced",
        sync_revision: 0,
      },
      {
        id: 9,
        name: "Second",
        task_count: 0,
        shared_role: "owner",
        sync_status: "synced",
        sync_revision: 0,
      },
    ];

    const creation = store.add_new_task(
      "column_1",
      create_task("", "New task"),
    );
    const deletion = store.delete_task(store.columns[0].tasks[0].id);
    await store.switch_board(9);
    add_task.resolve(42);

    await expect(creation).resolves.toBe(true);
    await expect(deletion).resolves.toBe(false);
    expect(invoke_mock).toHaveBeenCalledWith("delete_task", {
      taskId: 42,
      expectedBoardId: 7,
    });
  });

  it("keeps a debounced task move bound to the board where it began", async () => {
    vi.useFakeTimers();
    try {
      const add_task = deferred<number>();
      invoke_mock.mockImplementation((command) => {
        if (command === "add_task") {
          return add_task.promise;
        }
        if (command === "move_task" || command === "switch_board") {
          return Promise.resolve();
        }
        if (command === "get_columns") {
          return Promise.resolve([]);
        }
        if (command === "get_labels" || command === "get_task_templates") {
          return Promise.resolve([]);
        }
        return Promise.resolve();
      });
      const store = create_store(2);
      store.boards = [
        {
          id: 7,
          name: "First",
          task_count: 0,
          shared_role: "owner",
          sync_status: "synced",
          sync_revision: 0,
        },
        {
          id: 9,
          name: "Second",
          task_count: 0,
          shared_role: "owner",
          sync_status: "synced",
          sync_revision: 0,
        },
      ];
      const creation = store.add_new_task("column_1", create_task("", "Task"));

      expect(store.move_task(0, 0, 1, null)).toBe(true);
      // The timer has fired and is now waiting for the temporary card to
      // receive its database ID. Switching must not retarget that IPC.
      await vi.advanceTimersByTimeAsync(300);
      await store.switch_board(9);
      add_task.resolve(42);
      await expect(creation).resolves.toBe(true);
      await Promise.resolve();

      expect(invoke_mock).toHaveBeenCalledWith("move_task", {
        taskId: 42,
        toColumnId: 2,
        beforeTaskId: null,
        expectedBoardId: 7,
      });
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("column sorting", () => {
  it("keeps pinned cards first without applying sort rules to storage", () => {
    const store = create_store();
    const first = create_task("task_1", "First");
    const pinned = create_task("task_2", "Pinned");
    pinned.pinned = true;
    const dated = create_task(
      "task_3",
      "Dated",
      "",
      "",
      undefined,
      new Date(100),
    );
    store.columns[0].tasks = [first, pinned, dated];
    store.columns[0].sort_rules = [{ field: "due_time", direction: "asc" }];
    store.keep_pinned_tasks_first(store.columns[0]);
    expect(store.columns[0].tasks.map((task) => task.id)).toEqual([
      "task_2",
      "task_1",
      "task_3",
    ]);
    pinned.pinned = false;
    store.keep_pinned_tasks_first(store.columns[0]);
    expect(store.columns[0].tasks.map((task) => task.id)).toEqual([
      "task_2",
      "task_1",
      "task_3",
    ]);
  });
});

describe("invitation join errors", () => {
  it("uses the internal fallback for unknown failures and preserves approval errors", async () => {
    const store = new BoardStore();
    for (const failure of ["legacy private error", { code: "UNKNOWN" }, null]) {
      invoke_mock.mockReset();
      invoke_mock.mockRejectedValueOnce(failure);
      expect(await store.join_iroh_invite("ticket")).toBe(false);
      expect(store.iroh_last_error).toEqual({ code: "INTERNAL_ERROR" });
      expect(toast.error).toHaveBeenLastCalledWith(
        translateCommandError({ code: "INTERNAL_ERROR" }),
      );
    }
    const approval = { code: "SHARE_APPROVAL_REQUIRED", device_id: "device" };
    invoke_mock.mockRejectedValueOnce(approval);
    vi.mocked(toast.error).mockClear();
    expect(await store.join_iroh_invite("ticket")).toBe(false);
    expect(store.iroh_last_error).toEqual(approval);
    expect(toast.error).not.toHaveBeenCalled();
  });
});

describe("single-board import coordination", () => {
  afterEach(() => vi.restoreAllMocks());

  it.each([
    undefined,
    null,
    { layout: { nodes: [], viewport: null }, relationships: [] },
  ])(
    "offers Undo only for a legacy task-only backup (flow=%j)",
    async (flow) => {
      invoke_mock.mockReset();
      vi.mocked(toast.success).mockClear();
      invoke_mock.mockResolvedValue("isolated.pre-import.json");
      const store = create_store();
      store.column_fetch_finish = true;
      store.can_undo = true;
      vi.spyOn(store, "update_labels").mockImplementation(() => {});
      vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
      const columns = deferred<void>();
      vi.spyOn(store, "get_columns").mockImplementation(() => {
        expect(flow_imports.is_blocked(7)).toBe(true);
        expect(store.column_fetch_finish).toBe(false);
        return columns.promise;
      });
      const reload = vi.fn(async () => {});
      const unregister = flow_imports.register(7, {
        pause() {},
        reload,
        resume() {},
      });
      const imported = store.import_data(
        JSON.stringify({ columns: [], archives: [], flow }),
      );
      await vi.waitFor(() => expect(store.get_columns).toHaveBeenCalledOnce());
      expect(reload).not.toHaveBeenCalled();
      columns.resolve();
      expect(await imported).toBe(true);
      expect(reload).toHaveBeenCalledOnce();
      expect(store.can_undo).toBe(flow == null);
      expect(toast.success).toHaveBeenCalledWith(
        expect.any(String),
        expect.objectContaining({
          id: "data-import-success",
          action:
            flow == null
              ? expect.objectContaining({ onClick: expect.any(Function) })
              : undefined,
        }),
      );
      unregister();
    },
  );

  it("keeps the error screen after a post-import column read failure", async () => {
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) =>
      command === "get_columns"
        ? Promise.reject({ code: "INTERNAL_ERROR" })
        : Promise.resolve("isolated.pre-import.json"),
    );
    const store = create_store();
    store.column_fetch_finish = true;
    vi.spyOn(store, "update_labels").mockImplementation(() => {});
    vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
    expect(
      await store.import_data(
        '{"columns":[],"archives":[],"flow":{"layout":{"nodes":[],"viewport":null},"relationships":[]}}',
      ),
    ).toBe(true);
    expect(store.column_fetch_error).toBe(true);
    expect(store.column_fetch_finish).toBe(true);
    expect(store.can_undo).toBe(false);
    expect(flow_imports.is_blocked(7)).toBe(false);
  });

  it("replaces an archive request queued during import and permits subsequent fetches", async () => {
    const pending_import = deferred<string>();
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) =>
      command === "import_data" ? pending_import.promise : Promise.resolve([]),
    );
    const store = create_store();
    store.column_fetch_finish = true;
    vi.spyOn(store, "update_labels").mockImplementation(() => {});
    vi.spyOn(store, "get_task_templates").mockImplementation(() => {});
    const imported = store.import_data('{"columns":[],"archives":[]}');
    await vi.waitFor(() =>
      expect(invoke_mock).toHaveBeenCalledWith(
        "import_data",
        expect.anything(),
      ),
    );
    const obsolete = store.get_archives();
    expect(store.archives_loading).toBe(true);
    pending_import.resolve("isolated.pre-import.json");
    expect(await imported).toBe(true);
    await obsolete;
    await vi.waitFor(() => expect(store.archives_loaded).toBe(true));
    expect(store.archives_loading).toBe(false);
    expect(
      invoke_mock.mock.calls.filter(([command]) => command === "get_archives"),
    ).toHaveLength(2);
    await store.get_archives();
    expect(
      invoke_mock.mock.calls.filter(([command]) => command === "get_archives"),
    ).toHaveLength(3);
  });
});

describe("invalid backup input", () => {
  it("finishes backend validation before revoking links and importing", async () => {
    vi.mocked(open).mockResolvedValue("isolated-backup.json");
    const jsonData = '{"schema_version":1,"boards":[{"name":"Board"}]}';
    vi.mocked(readTextFile).mockResolvedValue(jsonData);
    const store = new BoardStore();
    const beforeRestore = vi.fn().mockResolvedValue(undefined);
    const calls: string[] = [];
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) => {
      calls.push(command);
      if (command === "validate_all_boards_backup") {
        expect(beforeRestore).not.toHaveBeenCalled();
      }
      if (command === "import_all_boards") {
        expect(beforeRestore).toHaveBeenCalledOnce();
      }
      return Promise.resolve({ notify_enabled: false });
    });
    vi.spyOn(store, "get_boards").mockResolvedValue(undefined);
    for (const method of [
      "get_columns",
      "update_labels",
      "get_task_templates",
    ] as const) {
      vi.spyOn(store, method).mockImplementation(() => {});
    }
    expect(await store.import_all_boards_from_file(false, beforeRestore)).toBe(
      true,
    );
    expect(calls).toEqual([
      "validate_all_boards_backup",
      "import_all_boards",
      "get_settings",
    ]);
    vi.restoreAllMocks();
  });

  it("reports invalid JSON and backup shapes before restoring or revoking links", async () => {
    vi.mocked(open).mockResolvedValue("isolated-backup.json");
    const store = new BoardStore();
    const beforeRestore = vi.fn();
    invoke_mock.mockReset();
    invoke_mock.mockImplementation((command) =>
      command === "validate_all_boards_backup"
        ? Promise.reject({ code: "INVALID_IMPORT" })
        : Promise.resolve(),
    );
    for (const json of [
      "{",
      "null",
      "[]",
      "{}",
      '{"schema_version":1,"boards":[]}',
      '{"schema_version":1,"boards":[{}]}',
    ]) {
      vi.mocked(readTextFile).mockResolvedValue(json);
      expect(
        await store.import_all_boards_from_file(false, beforeRestore),
      ).toBe(false);
      expect(toast.error).toHaveBeenLastCalledWith(
        translateCommandError({ code: "INVALID_IMPORT" }),
        { description: undefined },
      );
    }
    expect(beforeRestore).not.toHaveBeenCalled();
    expect(
      invoke_mock.mock.calls.some(
        ([command]) => command === "import_all_boards",
      ),
    ).toBe(false);
  });
});

describe("template copying", () => {
  it("copies to the explicit board without changing source templates or undo", async () => {
    invoke_mock.mockReset();
    invoke_mock.mockResolvedValue(undefined);
    const store = create_store();
    store.can_undo = true;
    const templates = store.templates;
    vi.spyOn(store, "get_boards").mockResolvedValue(undefined);
    expect(await store.copy_task_template_to_board(3, 8)).toBe(true);
    expect(invoke_mock).toHaveBeenCalledWith("copy_task_template_to_board", {
      expectedBoardId: 7,
      templateId: 3,
      targetBoardId: 8,
    });
    expect(store.templates).toBe(templates);
    expect(store.can_undo).toBe(true);
    invoke_mock.mockRejectedValue({ code: "PERMISSION_DENIED" });
    expect(await store.copy_task_template_to_board(3, 8)).toBe(false);
    expect(vi.mocked(toast.error)).toHaveBeenCalledWith(
      translateCommandError({ code: "PERMISSION_DENIED" }),
    );
    vi.restoreAllMocks();
  });
});
