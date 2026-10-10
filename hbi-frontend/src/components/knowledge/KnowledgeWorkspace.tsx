/**
 * Knowledge: one search over what the brain knows, seen four ways that share
 * a selection (the drawer). `#/knowledge?view=list|map|graph|timeline&q=&mode=&type=&hops=&within=`.
 *
 * - Results: the matches, each saying why it matched; entities named in the
 *   query alongside. With no query: the newest notes, or a drill-down from
 *   Home (`since`, `type`, `verified_since`).
 * - Neighbourhood: the graph around the results (or the open note).
 * - Map: the domains — topic hubs and the entities their notes mention (TopicMap);
 *   with `focus=<entity id>`, that entity's neighbourhood (EntityNeighbourhood).
 * - Timeline: when the results were written.
 * - References: which entities have a Wikipedia reference link (References).
 *
 * `within=<topic>` narrows everything to one topic.
 *
 * Search is non-perturbing: browsing never moves what retrieval ranks on.
 */
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { apiJson, query } from "../../api/client";
import { useSearch, type MatchTrace, type SearchMode, type SearchNote } from "../../api/search";
import { useTopics } from "../../api/topics";
import { noteTone } from "../../lib/noteTypes";
import { statusTone } from "../../lib/status";
import { formatLocal, parseTime, relTime } from "../../lib/time";
import { goTo } from "../../nav";
import { openEntity } from "../drawer/entities";
import { EntityLink } from "../drawer/parts";
import GraphPanel from "../graph/GraphPanel";
import { DrillBanner, EmptyState, Pill } from "../ui";
import TopicMap from "./TopicMap";
import EntityNeighbourhood from "./EntityNeighbourhood";
import References from "./References";

export interface KnowledgeParams {
  q?: string;
  mode?: string;
  type?: string;
  hops?: string;
  since?: string;
  verified_since?: string;
  within?: string;
  note?: string;
  entity?: string;
  topic?: string;
  /** Map: the entity whose neighbourhood is shown instead of the topic hubs. */
  focus?: string;
  /** Map neighbourhood: count only this topic's notes. */
  scope?: string;
  /** References: confirmed · proposed · none · waiting. */
  bucket?: string;
}

const MODES: { id: SearchMode; label: string; hint: string }[] = [
  { id: "auto", label: "Auto", hint: "A pasted id opens it; a \"quoted phrase\" is an exact phrase; anything else is semantic" },
  { id: "semantic", label: "Semantic", hint: "By meaning: the brain's own hybrid retrieval (vector + keyword, freshness-weighted)" },
  { id: "keyword", label: "Keyword", hint: "Every word must appear (BM25); quote a phrase to match it exactly" },
  { id: "id", label: "Id", hint: "An exact id: note, entity, task, job, schedule or todo" },
];

const TYPES = ["semantic", "episodic", "claim", "source_record", "inference", "reflection", "unsourced_synthesis", "outcome", "consolidated", "news"];

const BROWSE_CAP = 500;

export default function KnowledgeWorkspace({ view, params }: { view: string; params: KnowledgeParams }) {
  const q = params.q ?? "";
  const mode = (MODES.some((m) => m.id === params.mode) ? params.mode : "auto") as SearchMode;
  const hops = Number(params.hops ?? 0) || 0;
  // The input's text, tied to the query it was typed against: when the URL's
  // query changes (Back, Ctrl-K, a link) the input follows it.
  const [typed, setTyped] = useState({ forQ: q, text: q });
  const draft = typed.forQ === q ? typed.text : q;
  const setDraft = (text: string) => setTyped({ forQ: q, text });

  const run = (next: Partial<KnowledgeParams>) =>
    goTo("knowledge", {
      view,
      q: next.q ?? q,
      mode: (next.mode ?? mode) === "auto" ? undefined : next.mode ?? mode,
      type: next.type ?? params.type,
      hops: next.hops ?? (hops ? String(hops) : undefined),
      within: next.within ?? params.within,
      // Keep the open entity: a search doesn't close what you are reading.
      note: params.note,
      entity: params.entity,
      topic: params.topic,
    });
  const topics = useTopics();

  return (
    <div className="kn">
      <form className="kn-bar" onSubmit={(e) => { e.preventDefault(); run({ q: draft.trim() }); }}>
        <input
          className="ui-input kn-input"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Search by meaning, “an exact phrase”, or paste an id…"
          aria-label="Search knowledge"
        />
        <select className="ui-input auto" value={mode} onChange={(e) => run({ mode: e.target.value })}
          title={MODES.find((m) => m.id === mode)?.hint}>
          {MODES.map((m) => <option key={m.id} value={m.id} title={m.hint}>{m.label}</option>)}
        </select>
        <select className="ui-input auto" value={params.type ?? ""} onChange={(e) => run({ type: e.target.value })}>
          <option value="">All types</option>
          {TYPES.map((t) => <option key={t} value={t}>{t}</option>)}
        </select>
        <select className="ui-input auto" value={params.within ?? ""} onChange={(e) => run({ within: e.target.value })}
          title="Narrow to one topic (the domains on the Map)">
          <option value="">All topics</option>
          {topics.data?.topics.map((t) => <option key={t.name} value={t.name}>{t.label} ({t.notes.toLocaleString()})</option>)}
        </select>
        <label className="wk-check" title="Also add each match's graph neighbours, as the brain's search_notes does (2 hops)">
          <input type="checkbox" checked={hops > 0} onChange={(e) => run({ hops: e.target.checked ? "2" : "" })} /> + neighbours
        </label>
        <button type="submit" className="btn">Search</button>
        <button type="button" className="btn-ghost" onClick={() => openEntity("note", "new")}>+ New note</button>
      </form>
      {view === "references" ? (
        <References bucket={params.bucket} />
      ) : view === "map" && params.focus ? (
        <EntityNeighbourhood id={params.focus} scope={params.scope} />
      ) : view === "map" ? (
        <TopicMap />
      ) : q || params.within ? (
        <SearchBody view={view} q={q} mode={mode} type={params.type} hops={hops} within={params.within}
          openId={params.note ?? params.entity} />
      ) : (
        <BrowseBody view={view} params={params} />
      )}
    </div>
  );
}

// ── With a query ─────────────────────────────────────────────────────────────

function SearchBody({ view, q, mode, type, hops, within, openId }: {
  view: string; q: string; mode: SearchMode; type?: string; hops: number; within?: string; openId?: string;
}) {
  const limit = view === "timeline" ? 100 : 30;
  const s = useSearch({ q, mode, type, hops, limit, topic: within });
  const label = q ? `“${q}”` : `topic ${within}`;
  const d = s.data;

  // An id search opens what it found.
  useEffect(() => {
    if (d?.mode === "id" && d.id_match) openEntity(d.id_match.kind, d.id_match.id);
  }, [d]);

  if (!d) {
    return <div className="kn-body">{s.error ? <EmptyState tone="red" title="Search failed" checked={s.error.message} /> : <div className="loading">Searching…</div>}</div>;
  }

  if (d.mode === "id") {
    return (
      <div className="kn-body">
        {d.id_match
          ? <EmptyState title={`Opened the ${d.id_match.kind}`} checked={d.id_match.label ?? d.id_match.id} />
          : <EmptyState tone="yellow" title="Nothing has this id" checked={<>Checked notes, entities, tasks, jobs, schedules and todos for <code>{q}</code>.</>} />}
      </div>
    );
  }

  const header = (
    <div className="kn-summary">
      <span>
        <strong>{d.notes.length}{d.truncated ? "+" : ""}</strong> notes {d.mode === "topic" ? <>in topic <strong>{d.topic}</strong>, newest first</> : <>by <strong>{d.mode}</strong></>}
        {d.type && <> · type <strong>{d.type}</strong></>}
        {d.topic && d.mode !== "topic" && <> · within <strong>{d.topic}</strong></>}
        {d.topic_filtered && <span className="wk-dim"> ({d.topic_filtered.in_topic} of the {d.topic_filtered.searched} best matches are in the topic)</span>}
        {d.requested_mode === "auto" && d.mode !== "topic" && <span className="wk-dim"> (auto: {d.mode === "keyword" ? "a quoted phrase" : "a phrase"})</span>}
      </span>
      {d.truncated && <span className="tone-text-yellow">the best {d.limit}; there are more</span>}
      {d.vector_available === false && <span className="tone-text-yellow">the query could not be embedded — these are keyword matches only</span>}
    </div>
  );

  const entities = d.entities.length > 0 && (
    <div className="kn-entities">
      <span className="wk-dim">Entities</span>
      {d.entities.map((e) => (
        <EntityLink key={e.id} kind="entity" id={e.id} title={`${e.entity_type ?? ""} · ${e.mentions} notes`}>
          <Pill tone="orange">{e.name} {e.mentions}</Pill>
        </EntityLink>
      ))}
    </div>
  );

  if (d.error) {
    return <div className="kn-body"><EmptyState tone="red" title="Search failed" checked={d.error} /></div>;
  }

  if (view === "graph") {
    const seeds = d.notes.slice(0, 30).map((n) => n.id);
    return (
      <div className="kn-body kn-graph">
        {header}
        {seeds.length === 0
          ? <EmptyState title="No matches to draw" checked={`Nothing matched “${q}”.`} />
          : <GraphPanel seedIds={seeds} seedLabel={`${label} (${seeds.length} ${q ? "best matches" : "newest notes"})`} selectedId={openId} />}
      </div>
    );
  }

  if (view === "timeline") {
    return (
      <div className="kn-body scroll">
        {header}
        <Timeline notes={d.notes} caption={`When the ${d.notes.length} ${q ? `best matches for ${label}` : `newest notes in ${label}`} were written`} openId={openId} />
      </div>
    );
  }

  return (
    <div className="kn-body scroll">
      {header}
      {entities}
      {d.notes.length === 0 ? (
        <EmptyState
          title="No notes match"
          checked={d.mode === "topic"
            ? `No note is in topic ${d.topic} yet. Notes are classified while the brain is idle (the Map shows how far along it is).`
            : d.mode === "keyword"
            ? `No note contains every word of “${q}”${d.type ? ` among ${d.type} notes` : ""}${d.topic ? ` within ${d.topic}` : ""}. Try Semantic to search by meaning.`
            : `Nothing matched “${q}”${d.type ? ` among ${d.type} notes` : ""}${d.topic ? ` within ${d.topic} (${d.topic_filtered?.searched ?? 0} matches searched)` : ""}.`}
        />
      ) : (
        d.notes.map((n) => <ResultRow key={n.id} n={n} active={openId === n.id} />)
      )}
    </div>
  );
}

function MatchChips({ m }: { m: MatchTrace }) {
  const chips: ReactNode[] = [];
  if (m.vector_rank) chips.push(<span key="v" title="Vector similarity (meaning): rank in the vector search, cosine score">meaning #{m.vector_rank} · {m.vector_score?.toFixed(2)}</span>);
  if (m.bm25_rank) chips.push(<span key="b" title="BM25 keyword relevance: rank in the fulltext search, Lucene score">words #{m.bm25_rank} · {m.bm25_score?.toFixed(1)}</span>);
  if (m.via_chunk) chips.push(<span key="c" title="The match was in one chunk of this longer note">in a chunk</span>);
  if (m.added_by === "graph") chips.push(<span key="g" className="tone-text-purple" title="Not a match: a graph neighbour of one">graph neighbour</span>);
  if (m.added_by === "entity") chips.push(<span key="e" className="tone-text-purple" title="Not a match: mentions the same entities as one">shares entities</span>);
  if (m.added_by === "substring_fallback") chips.push(<span key="s" className="tone-text-yellow" title="Vector and keyword search found nothing; this is a plain substring match">substring</span>);
  return <span className="kn-match">{chips}</span>;
}

function ResultRow({ n, active }: { n: SearchNote; active: boolean }) {
  return (
    <button type="button" className={`kn-row${active ? " active" : ""}`} onClick={() => openEntity("note", n.id)}>
      <span className="kn-row-head">
        <span className="kn-rank">{n.match.rank}</span>
        <Pill tone={noteTone(n.note_type)}>{n.note_type ?? "note"}</Pill>
        {n.claim_status && <Pill tone={statusTone(n.claim_status)}>{n.claim_status}</Pill>}
        {n.verification === "failed" && <Pill tone="red">verification failed</Pill>}
        <MatchChips m={n.match} />
        {n.topics?.map((t, i) => (
          <span key={t} role="link" tabIndex={0} className={`kn-topic${i > 0 ? " second" : ""}`} title={i === 0 ? "Main topic" : "Second topic"}
            onClick={(e) => { e.stopPropagation(); openEntity("topic", t); }}
            onKeyDown={(e) => { if (e.key === "Enter") { e.stopPropagation(); openEntity("topic", t); } }}>{t}</span>
        ))}
        <span className="wk-dim kn-when" title={formatLocal(n.created_at)}>{relTime(n.created_at)}{n.source_context ? ` · ${n.source_context}` : ""}</span>
      </span>
      <span className="kn-preview">{n.preview}{n.preview_truncated ? "…" : ""}</span>
    </button>
  );
}

// ── Without a query ──────────────────────────────────────────────────────────

interface BrowseNote {
  id: string;
  content: string;
  note_type?: string | null;
  created_at?: string;
}

function BrowseBody({ view, params }: { view: string; params: KnowledgeParams }) {
  const drilled = !!(params.since || params.verified_since || params.type);
  const limit = drilled || view === "timeline" ? BROWSE_CAP : 60;
  const list = useQuery({
    queryKey: ["knowledge", "browse", params.since ?? "", params.type ?? "", params.verified_since ?? "", limit],
    queryFn: async () =>
      (await apiJson<{ notes?: BrowseNote[] }>(`/api/notes${query({
        since: params.since, note_type: params.type, verified_since: params.verified_since, limit,
      })}`)).notes ?? [],
    staleTime: 30_000,
  });
  const openId = params.note ?? params.entity;

  if (view === "graph") {
    return (
      <div className="kn-body kn-graph">
        {params.note
          ? <GraphPanel seedIds={[params.note]} seedLabel="the open note" selectedId={openId} />
          : <GraphPanel selectedId={openId} />}
      </div>
    );
  }

  const banner = drilled && (
    <DrillBanner rows={list.data?.length} cap={limit} onClear={() => goTo("knowledge", { view })}>
      {params.type ? <strong>{params.type}</strong> : "all"} notes
      {params.since && <> created since {formatLocal(params.since)}</>}
      {params.verified_since && <> checked since {formatLocal(params.verified_since)}</>}
    </DrillBanner>
  );

  if (!list.data) {
    return <div className="kn-body">{banner}{list.error ? <EmptyState tone="red" title="Couldn't list notes" checked={list.error.message} /> : <div className="loading">Loading notes…</div>}</div>;
  }
  const notes: SearchNote[] = list.data.map((n, i) => ({
    id: n.id,
    note_type: n.note_type,
    preview: n.content.slice(0, 320),
    preview_truncated: n.content.length > 320,
    created_at: n.created_at,
    match: { rank: i + 1 },
  }));

  if (view === "timeline") {
    return (
      <div className="kn-body scroll">
        {banner}
        <Timeline notes={notes} caption={drilled ? `When these ${notes.length} notes were written` : `The newest ${notes.length} notes`} openId={openId} />
      </div>
    );
  }

  return (
    <div className="kn-body scroll">
      {banner}
      {!drilled && <div className="kn-summary"><span className="wk-dim">The newest {notes.length} notes. Search above, or open one.</span></div>}
      {notes.length === 0
        ? <EmptyState title="No notes" checked="No note matches these filters." />
        : notes.map((n) => (
          <button key={n.id} type="button" className={`kn-row${openId === n.id ? " active" : ""}`} onClick={() => openEntity("note", n.id)}>
            <span className="kn-row-head">
              <Pill tone={noteTone(n.note_type)}>{n.note_type ?? "note"}</Pill>
              <span className="wk-dim kn-when" title={formatLocal(n.created_at)}>{relTime(n.created_at)}</span>
            </span>
            <span className="kn-preview">{n.preview}{n.preview_truncated ? "…" : ""}</span>
          </button>
        ))}
    </div>
  );
}

// ── Timeline ─────────────────────────────────────────────────────────────────

const DAY = 86_400_000;

/** Notes per day (or per week over a long span); a bar lists its notes below. */
function Timeline({ notes, caption, openId }: { notes: SearchNote[]; caption: string; openId?: string }) {
  const [picked, setPicked] = useState<number | null>(null);
  const dated = notes
    .map((n) => ({ n, t: parseTime(n.created_at)?.getTime() }))
    .filter((x): x is { n: SearchNote; t: number } => x.t !== undefined);
  if (dated.length === 0) return <EmptyState title="Nothing to place on a timeline" checked="None of these notes has a creation time." />;

  const min = Math.min(...dated.map((x) => x.t));
  const max = Math.max(...dated.map((x) => x.t));
  const bucket = max - min > 90 * DAY ? 7 * DAY : DAY;
  const start = Math.floor(min / bucket) * bucket;
  const count = Math.max(1, Math.floor((max - start) / bucket) + 1);
  const buckets: SearchNote[][] = Array.from({ length: count }, () => []);
  for (const { n, t } of dated) buckets[Math.floor((t - start) / bucket)].push(n);
  const peak = Math.max(...buckets.map((b) => b.length));
  const label = (i: number) => new Date(start + i * bucket).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  const shown = picked !== null ? buckets[picked] ?? [] : [];

  return (
    <div className="kn-tl">
      <div className="kn-summary">
        <span>{caption}, per {bucket === DAY ? "day" : "week"}.</span>
        {dated.length < notes.length && <span className="wk-dim">{notes.length - dated.length} without a date are left out.</span>}
      </div>
      <div className="kn-tl-bars" role="list">
        {buckets.map((b, i) => (
          <button key={i} type="button" role="listitem"
            className={`kn-tl-bar${picked === i ? " active" : ""}`}
            style={{ height: `${b.length ? Math.max(6, (b.length / peak) * 100) : 0}%` }}
            title={`${label(i)}: ${b.length} notes`}
            onClick={() => setPicked(picked === i ? null : i)} />
        ))}
      </div>
      <div className="kn-tl-axis"><span>{label(0)}</span><span>{label(count - 1)}</span></div>
      {picked !== null && (
        <div className="kn-tl-list">
          <div className="wk-dim">{label(picked)} · {shown.length} notes</div>
          {shown.map((n) => (
            <button key={n.id} type="button" className={`kn-row${openId === n.id ? " active" : ""}`} onClick={() => openEntity("note", n.id)}>
              <span className="kn-row-head"><Pill tone={noteTone(n.note_type)}>{n.note_type ?? "note"}</Pill>
                <span className="wk-dim kn-when">{formatLocal(n.created_at)}</span></span>
              <span className="kn-preview">{n.preview}</span>
            </button>
          ))}
        </div>
      )}
      {picked === null && <div className="wk-hint">Click a bar to list its notes.</div>}
    </div>
  );
}
