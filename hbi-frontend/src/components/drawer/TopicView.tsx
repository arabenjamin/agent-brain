/**
 * A topic: what it is (its description is also the classifier's definition),
 * how many notes it holds, what they mention most, the newest of them, and the
 * schedules that watch it. "Search within" narrows Knowledge to the topic.
 * Edit, delete and reset it here (§10.2); `name = "new"` creates one.
 */
import { useState } from "react";
import { ApiError } from "../../api/client";
import { deleteTopic, resetTopic, setEntityHidden, useTopic } from "../../api/topics";
import { noteTone } from "../../lib/noteTypes";
import { relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { Drawer, EmptyState, Pill, Stat } from "../ui";
import { closeEntity, openEntity } from "./entities";
import TopicForm from "./TopicForm";
import { EntityLink, LoadState, Section } from "./parts";

function NewTopic() {
  return (
    <Drawer title="New topic" onClose={closeEntity} wide>
      <div className="dw-note">
        Every note is classified again against all topics once it is created, while the brain is idle (about 4 hours for the whole graph). Keep the
        definition short: every topic's definition shares one local-model prompt.
      </div>
      <TopicForm existing={null} onCancel={closeEntity} onSaved={(n) => openEntity("topic", n)} />
    </Drawer>
  );
}

export default function TopicView({ name }: { name: string }) {
  if (name === "new") return <NewTopic />;
  return <OneTopic name={name} />;
}

function OneTopic({ name }: { name: string }) {
  const q = useTopic(name);
  const d = q.data;
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const act = async (f: () => Promise<string>) => {
    setBusy(true);
    setMsg(null);
    try {
      setMsg({ ok: true, text: await f() });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };
  if (!d) {
    const missing = q.error instanceof ApiError && q.error.status === 404;
    return (
      <Drawer title="Topic" onClose={closeEntity}>
        {missing
          ? <EmptyState tone="yellow" title="No topic with this name" checked={<>Looked for <code>{name}</code>. Topics come from <code>topics/*.yaml</code>; it may have been renamed or removed.</>} />
          : <LoadState what="topic" id={name} error={q.error} onRetry={() => q.refetch()} />}
      </Drawer>
    );
  }
  const t = d.topic;
  return (
    <Drawer title={<><span>Topic</span><Pill tone="accent">{t.name}</Pill></>} onClose={closeEntity} wide>
      <h3 className="dw-title">{t.label}</h3>
      {editing ? (
        <TopicForm
          existing={{ name: t.name, label: t.label, description: t.description, source_lists: t.source_lists, assign: t.assign, members: t.members }}
          onCancel={() => setEditing(false)}
          onSaved={(_, note) => { setEditing(false); setMsg({ ok: true, text: note }); }}
        />
      ) : (
        <div className="dw-note">{t.description}</div>
      )}
      {t.assign === "rules" && (
        <div className="dw-note">
          <b>Assigned by rules, not by the classifier.</b> A note joins when it matches:{" "}
          {t.members.length === 0 ? "no rule (only notes you add by hand)" : t.members.map((r, i) => (
            <span key={i}>
              {i > 0 && " · or "}
              {[r.source_context && `source ${r.source_context}`, r.note_type && `type ${r.note_type}`, r.content_contains && `text contains “${r.content_contains}”`].filter(Boolean).join(" and ")}
            </span>
          ))}
          ; or you add it from a note's drawer.
        </div>
      )}
      <div className="dw-dim">
        {t.managed_by === "runtime"
          ? <>Edited here{t.updated_at ? ` ${relTime(t.updated_at)}` : ""}{d.seed ? <>; it shadows <code>topics/*.yaml</code>, which no longer applies</> : "; no YAML file declares it"}.</>
          : <>Defined in <code>topics/*.yaml</code>; an edit here takes over from the file.</>}
        {d.reclassify.waiting > 0 && <> · <span className="tone-text-yellow">{d.reclassify.waiting.toLocaleString()} notes waiting to be classified under the current definitions</span></>}
      </div>
      {!editing && (
        <div className="item-actions">
          <button type="button" className="btn-ghost" disabled={busy} onClick={() => setEditing(true)}>Edit definition</button>
          {t.managed_by === "runtime" && d.seed && (
            <button type="button" className="btn-ghost" disabled={busy} title={d.seed.description}
              onClick={() => act(async () => { await resetTopic(t.name); return "Back to its YAML definition."; })}>Reset to YAML</button>
          )}
          <button type="button" className="btn-ghost" disabled={busy}
            title={d.schedules.length ? "Schedules declare it: give them another topic first" : "Its notes are classified again among the other topics"}
            onClick={() => {
              if (!window.confirm(`Delete the topic "${t.label}"? Its ${t.notes.toLocaleString()} notes are classified again among the other topics.`)) return;
              act(async () => { await deleteTopic(t.name); closeEntity(); return "Deleted."; });
            }}>Delete…</button>
          {msg && <span className={msg.ok ? "tone-text-green" : "tone-text-red"} style={{ fontSize: 11 }}>{msg.text}</span>}
        </div>
      )}
      <div className="ui-stats">
        <Stat label="Notes" value={t.notes.toLocaleString()} title={`${t.main.toLocaleString()} have it as their main topic`} />
        <Stat label="Main topic of" value={t.main.toLocaleString()} />
        <Stat label="This week" value={t.week.toLocaleString()} />
      </div>
      <div className="settings-actions">
        <a className="btn" href={hrefTo("knowledge", { view: "list", within: t.name })}>Search within this topic</a>
        <a className="btn-ghost" href={hrefTo("knowledge", { view: "map" })}>On the map</a>
      </div>

      <Section title="Watched by" aside={d.schedules.length ? `${d.schedules.length}` : undefined}>
        {d.schedules.length === 0 ? (
          <div className="dw-note">No schedule declares this topic (a schedule's <code>topic:</code>, set in its YAML or the schedule form).</div>
        ) : (
          <ul className="dw-list">
            {d.schedules.map((s) => (
              <li key={s.name}>
                {s.id ? <EntityLink kind="schedule" id={s.id}>{s.name}</EntityLink> : <span>{s.name}</span>}
                {s.kind && <span className="dw-dim">{s.kind}</span>}
                {s.enabled === false && <Pill tone="muted">disabled</Pill>}
              </li>
            ))}
          </ul>
        )}
        {t.source_lists.length > 0 && <div className="dw-dim">Searches use: {t.source_lists.join(", ")}</div>}
      </Section>

      <Section title="Mentioned most" aside="by notes in this topic">
        {d.entities.length === 0
          ? <div className="dw-note">Its notes mention no entities (urls and dates are not counted).</div>
          : (
            <div className="dw-chips">
              {d.entities.map((e) => (
                <span key={e.id} className="note-topic">
                  <EntityLink kind="entity" id={e.id} title={`${e.entity_type ?? ""} · ${e.notes} notes in this topic`}>
                    <Pill tone="muted">{e.name} {e.notes}</Pill>
                  </EntityLink>
                  <button type="button" className="btn-ghost note-topic-x" disabled={busy}
                    title={`Keep ${e.name} out of ${t.label}: off its map and this list; its notes are unchanged`}
                    onClick={() => act(async () => { await setEntityHidden(e.id, t.name, true); return `${e.name} kept out of ${t.label}.`; })}>×</button>
                </span>
              ))}
            </div>
          )}
      </Section>

      {d.hidden.length > 0 && (
        <Section title="Kept out by you" aside={`${d.hidden.length}`}>
          <ul className="dw-list">
            {d.hidden.map((e) => (
              <li key={e.id}>
                <EntityLink kind="entity" id={e.id}>{e.name}</EntityLink>
                <span className="dw-dim">{e.notes} of this topic's notes mention it{e.at ? ` · hidden ${relTime(e.at)}` : ""}</span>
                <button type="button" className="btn-ghost" disabled={busy}
                  onClick={() => act(async () => { await setEntityHidden(e.id, t.name, false); return `${e.name} is back in ${t.label}.`; })}>Let back in</button>
              </li>
            ))}
          </ul>
        </Section>
      )}

      <Section title="Newest notes" aside={t.notes > d.notes_shown ? `${d.notes_shown} of ${t.notes.toLocaleString()}` : `${d.notes_shown}`}>
        {d.notes.length === 0 ? (
          <EmptyState title="No notes yet" checked="Notes are classified while the brain is idle; see the coverage line on the map." />
        ) : (
          <ul className="dw-list">
            {d.notes.map((n) => (
              <li key={n.id}>
                <EntityLink kind="note" id={n.id}><Pill tone={noteTone(n.note_type)}>{n.note_type ?? "note"}</Pill></EntityLink>
                <span className="dw-clip">{n.preview}</span>
                <span className="dw-dim">{n.main ? "" : "second topic · "}{relTime(n.created_at)}</span>
              </li>
            ))}
          </ul>
        )}
      </Section>
    </Drawer>
  );
}
