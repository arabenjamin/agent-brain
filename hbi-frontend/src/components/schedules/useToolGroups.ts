import { useEffect, useState } from "react";
import { apiRequest } from "../../api/client";
import { TOOL_GROUPS, type ToolGroup } from "./model";

// The dropdown used to drift: a newly-registered tool (e.g. notify_user) would be
// missing until someone hand-edited TOOL_GROUPS. This merges the live tool set on
// top of the curated groups. Note: tools/list is bounded by MCP_TOOL_PROFILE (often
// `general`), so it is NOT a superset — hence a FLOOR merge, never a replacement:
// every curated tool always shows, and any live tool not already grouped lands in
// "Other (live)". Offline (fetch fails) it degrades to the curated groups.
let cachedLiveTools: string[] | null = null;

export function useMergedToolGroups(): ToolGroup[] {
  const [groups, setGroups] = useState<ToolGroup[]>(TOOL_GROUPS);

  useEffect(() => {
    let cancelled = false;
    const apply = (live: string[]) => {
      if (cancelled) return;
      const known = new Set(TOOL_GROUPS.flatMap((g) => g.tools));
      const extra = live.filter((t) => t && !known.has(t)).sort();
      setGroups(extra.length ? [...TOOL_GROUPS, { group: "Other (live)", tools: extra }] : TOOL_GROUPS);
    };

    if (cachedLiveTools) { apply(cachedLiveTools); return; }

    (async () => {
      // Prefer /api/tools — the COMPLETE registered set, unfiltered by
      // MCP_TOOL_PROFILE. Fall back to the (profile-bounded) MCP tools/list on an
      // older backend that lacks the route.
      try {
        const res = await apiRequest(`/api/tools`, { method: "GET" });
        if (res.ok) {
          const data = await res.json();
          const names: string[] = (data?.tools ?? []).filter(Boolean);
          if (names.length) { cachedLiveTools = names; apply(names); return; }
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
        cachedLiveTools = names;
        apply(names);
      } catch { /* offline — keep the curated groups */ }
    })();

    return () => { cancelled = true; };
  }, []);

  return groups;
}

