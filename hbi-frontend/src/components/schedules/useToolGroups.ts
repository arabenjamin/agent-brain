import { useEffect, useState } from "react";
import { apiRequest } from "../../api/client";
import { TOOL_GROUPS, type ToolGroup } from "./model";

// The dropdown used to drift: a newly-registered tool (e.g. notify_user) would be
// missing until someone hand-edited TOOL_GROUPS. This merges the live tool set on
// top of the curated groups, and any live tool not already grouped lands in
// "Other (live)". /api/tools is the COMPLETE registry, so with it the curated
// groups are also FILTERED to what exists: a removed tool must not be offered
// (CORE_AND_PLUGINS_PLAN.md §2.9). tools/list is bounded by MCP_TOOL_PROFILE, so
// on that fallback (or offline) the curated groups stay whole.
let cachedLiveTools: { names: string[]; complete: boolean } | null = null;

export function useMergedToolGroups(): ToolGroup[] {
  const [groups, setGroups] = useState<ToolGroup[]>(TOOL_GROUPS);

  useEffect(() => {
    let cancelled = false;
    const apply = (live: string[], complete: boolean) => {
      if (cancelled) return;
      const liveSet = new Set(live);
      const curated = complete
        ? TOOL_GROUPS.map((g) => ({ ...g, tools: g.tools.filter((t) => liveSet.has(t)) }))
            .filter((g) => g.tools.length)
        : TOOL_GROUPS;
      const known = new Set(TOOL_GROUPS.flatMap((g) => g.tools));
      const extra = live.filter((t) => t && !known.has(t)).sort();
      setGroups(extra.length ? [...curated, { group: "Other (live)", tools: extra }] : curated);
    };

    if (cachedLiveTools) { apply(cachedLiveTools.names, cachedLiveTools.complete); return; }

    (async () => {
      // Prefer /api/tools — the COMPLETE registered set, unfiltered by
      // MCP_TOOL_PROFILE. Fall back to the (profile-bounded) MCP tools/list on an
      // older backend that lacks the route.
      try {
        const res = await apiRequest(`/api/tools`, { method: "GET" });
        if (res.ok) {
          const data = await res.json();
          const names: string[] = (data?.tools ?? []).filter(Boolean);
          if (names.length) { cachedLiveTools = { names, complete: true }; apply(names, true); return; }
        }
      } catch { /* fall through to tools/list */ }

      try {
        const res = await apiRequest(`/mcp`, {
          method: "POST",
          body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
        });
        if (!res.ok) return;
        const data = await res.json();
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const names: string[] = (data?.result?.tools ?? []).map((t: any) => t?.name).filter(Boolean);
        cachedLiveTools = { names, complete: false };
        apply(names, false);
      } catch { /* offline — keep the curated groups */ }
    })();

    return () => { cancelled = true; };
  }, []);

  return groups;
}

