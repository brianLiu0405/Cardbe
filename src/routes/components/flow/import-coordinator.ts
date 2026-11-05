/** A board replacement must follow every already-issued canvas write, including
 * writes from views that have unmounted. The gate stays closed during reload. */
export interface ImportView {
  pause(): void;
  reload(): Promise<void>;
  resume(): void;
}

export class FlowImportCoordinator {
  private blocked = new Set<number>();
  private writes = new Map<number, Set<Promise<unknown>>>();
  private views = new Map<number, Set<ImportView>>();

  is_blocked(board: number) {
    return this.blocked.has(board);
  }

  register(board: number, view: ImportView) {
    const views = this.views.get(board) ?? new Set<ImportView>();
    this.views.set(board, views);
    views.add(view);
    if (this.is_blocked(board)) {
      view.pause();
    }
    return () => {
      views.delete(view);
      if (!views.size) {
        this.views.delete(board);
      }
    };
  }

  write<T>(board: number, action: () => Promise<T>): Promise<T> {
    if (this.is_blocked(board)) {
      return Promise.reject({ code: "STALE_BOARD_REQUEST" });
    }
    const writes = this.writes.get(board) ?? new Set<Promise<unknown>>();
    this.writes.set(board, writes);
    // Invoke immediately: deferred invocation could otherwise cross the gate.
    const result = action().finally(() => {
      writes.delete(result);
      if (!writes.size) {
        this.writes.delete(board);
      }
    });
    writes.add(result);
    return result;
  }

  async replace<T>(board: number, action: () => Promise<T>): Promise<T> {
    if (this.is_blocked(board)) {
      throw { code: "STALE_BOARD_REQUEST" };
    }
    this.blocked.add(board);
    try {
      for (const view of this.views.get(board) ?? []) {
        view.pause();
      }
      await Promise.allSettled(this.writes.get(board) ?? []);
      return await action();
    } finally {
      // A view may mount while another view's reload is pending.
      const reloaded = new Set<ImportView>();
      for (;;) {
        const pending = [...(this.views.get(board) ?? [])].filter(
          (view) => !reloaded.has(view),
        );
        if (!pending.length) {
          break;
        }
        pending.forEach((view) => reloaded.add(view));
        await Promise.allSettled(pending.map((view) => view.reload()));
      }
      this.blocked.delete(board);
      for (const view of this.views.get(board) ?? []) {
        view.resume();
      }
    }
  }
}

export const flow_imports = new FlowImportCoordinator();
