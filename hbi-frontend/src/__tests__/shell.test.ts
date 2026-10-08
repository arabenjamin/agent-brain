import { describe, expect, it } from "vitest";
import { resolveRoute, LEGACY_PATHS } from "../workspaces";
import { summariseScheduler } from "../lib/scheduler";
import { matchCommands, paletteResults, queryCommands, staticCommands } from "../components/palette/commands";
import { hrefTo, keptParams } from "../nav";
import type { SchedulerStatus } from "../api/queries";

describe("resolveRoute", () => {
  it("shows a workspace's first view when none is given", () => {
    const r = resolveRoute({ path: "work", params: {} });
    expect(r.kind === "show" && [r.workspace.id, r.view?.id]).toEqual(["work", "jobs"]);
  });

  it("shows the named view", () => {
    const r = resolveRoute({ path: "knowledge", params: { view: "graph" } });
    expect(r.kind === "show" && r.view?.id).toBe("graph");
  });

  it("falls back to the first view for an unknown view without redirecting", () => {
    const r = resolveRoute({ path: "system", params: { view: "nope" } });
    expect(r.kind === "show" && r.view?.id).toBe("logs");
  });

  it("redirects every old tab URL to its new view", () => {
    for (const [old, target] of Object.entries(LEGACY_PATHS)) {
      const r = resolveRoute({ path: old, params: { note: "n1" } });
      expect(r).toEqual({ kind: "redirect", path: target.path, params: target.view ? { view: target.view } : {} });
    }
  });

  it("sends an unknown path home", () => {
    expect(resolveRoute({ path: "bogus", params: {} })).toEqual({ kind: "redirect", path: "home", params: {} });
    expect(resolveRoute({ path: "", params: {} })).toEqual({ kind: "redirect", path: "home", params: {} });
  });

  it("keeps knowledge, chat, home and settings as they are", () => {
    for (const path of ["knowledge", "chat", "home", "settings"]) {
      expect(resolveRoute({ path, params: {} }).kind).toBe("show");
    }
  });
});

function status(config: Partial<SchedulerStatus["config"]>, state: Partial<SchedulerStatus["state"]>): SchedulerStatus {
  return {
    config: {
      enabled: true, interval_secs: 300, sleep_interval_secs: 1800, idle_sleep_after_ticks: 3,
      max_tasks_per_run: 3, error_budget: 5, session_id: null, ...config,
    },
    state: {
      is_running: false, is_sleeping: false, idle_ticks: 0, tasks_dispatched: 0, consecutive_errors: 0,
      last_run_at: null, last_activity_at: null, last_error: null, ...state,
    },
  };
}

describe("summariseScheduler", () => {
  it("reads a sleeping scheduler between ticks as on, not paused (the old bug)", () => {
    const s = summariseScheduler(status({}, { is_running: false, is_sleeping: true }));
    expect(s).toMatchObject({ phase: "sleeping", enabled: true });
  });

  it("is paused only when config.enabled is false", () => {
    expect(summariseScheduler(status({ enabled: false }, {})).phase).toBe("paused");
  });

  it("names an auto-pause from the error budget", () => {
    const s = summariseScheduler(status({ enabled: false, error_budget: 5 }, { consecutive_errors: 5, last_error: "boom" }));
    expect(s.phase).toBe("auto-paused");
    expect(s.detail).toContain("boom");
  });

  it("reports a tick in progress, then errors, then idle", () => {
    expect(summariseScheduler(status({}, { is_running: true })).phase).toBe("ticking");
    expect(summariseScheduler(status({}, { consecutive_errors: 2 })).label).toBe("2/5 errors");
    expect(summariseScheduler(status({}, {})).phase).toBe("idle");
  });
});

describe("palette commands", () => {
  const all = staticCommands();

  it("covers every workspace view and settings section", () => {
    const ids = all.map((c) => c.id);
    for (const id of ["go:work/scheduled", "go:knowledge/graph", "go:system/logs", "settings:scheduler", "settings:agents"]) {
      expect(ids).toContain(id);
    }
  });

  it("ranks a label prefix above a keyword-only match", () => {
    const r = matchCommands(all, "sched");
    expect(r[0].label).toContain("Scheduled");
    expect(r.map((c) => c.id)).toContain("settings:scheduler");
  });

  it("requires every typed word", () => {
    // "budget" alone would also match the scheduler's error budget.
    const r = matchCommands(all, "settings spend");
    expect(r.map((c) => c.id)).toEqual(["settings:models"]);
  });

  it("returns everything in order for an empty query", () => {
    expect(matchCommands(all, "  ")).toEqual(all);
  });

  it("puts real matches before 'search notes', and a pasted id first", () => {
    expect(paletteResults(all, "sched")[0].id).toBe("go:work/scheduled");
    expect(paletteResults(all, "sched").at(-1)?.id).toBe("search:sched");
    expect(paletteResults(all, "867f7a9d-3b11-4158-ba12-b6f167c0f49a")[0].id).toMatch(/^note:/);
    // Nothing else matches: the search is the only (and first) result.
    expect(paletteResults(all, "zxqv").map((c) => c.id)).toEqual(["search:zxqv"]);
  });

  it("turns a pasted id into 'open note' and a phrase into a search", () => {
    expect(queryCommands("867f7a9d-3b11-4158-ba12-b6f167c0f49a")[0].label).toContain("Open note");
    expect(queryCommands("donald trump")[0].label).toBe("Search notes for “donald trump”");
    expect(queryCommands("   ")).toEqual([]);
  });
});

describe("nav", () => {
  it("keeps only the open drawer entity across a move", () => {
    expect(keptParams({ note: "n1", since: "x", view: "jobs" })).toEqual({ note: "n1" });
  });

  it("builds hrefs without empty params", () => {
    expect(hrefTo("work", { view: "jobs", since: undefined, status: "" })).toBe("#/work?view=jobs");
  });
});
