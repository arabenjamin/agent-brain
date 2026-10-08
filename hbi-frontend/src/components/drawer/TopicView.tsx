/**
 * A topic: what it is (its YAML description is also the classifier's
 * definition), how many notes it holds, what they mention most, the newest of
 * them, and the schedules that watch it. "Search within" narrows Knowledge
 * to the topic.
 */
import { ApiError } from "../../api/client";
import { useTopic } from "../../api/topics";
import { noteTone } from "../../lib/noteTypes";
import { relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { Drawer, EmptyState, Pill, Stat } from "../ui";
import { closeEntity } from "./entities";
import { EntityLink, LoadState, Section } from "./parts";

export default function TopicView({ name }: { name: string }) {
  const q = useTopic(name);
  const d = q.data;
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
      <div className="dw-note">{t.description}</div>
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
          <div className="dw-note">No schedule is linked to this topic in its YAML (<code>schedules:</code>).</div>
        ) : (
          <ul className="dw-list">
            {d.schedules.map((s) => (
              <li key={s.name}>
                {s.id ? <EntityLink kind="schedule" id={s.id}>{s.name}</EntityLink> : <span>{s.name}</span>}
                {s.missing && <span className="tone-text-yellow">no ScheduledTask has this name</span>}
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
                <EntityLink key={e.id} kind="entity" id={e.id} title={`${e.entity_type ?? ""} · ${e.notes} notes in this topic`}>
                  <Pill tone="muted">{e.name} {e.notes}</Pill>
                </EntityLink>
              ))}
            </div>
          )}
      </Section>

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
