/**
 * The items tagged with this thing (`GET /api/refs/todos`): the work that
 * serves or benefits it, open first. Shown in the workflow, program, topic
 * and entity drawers.
 */
import { useTodosFor } from "../../api/refs";
import { EntityLink, Section } from "./parts";

export default function TodosFor({ tag, what }: { tag: string; what: string }) {
  const q = useTodosFor(tag);
  const todos = q.data?.todos ?? [];
  const open = todos.filter((t) => t.status !== "done").length;
  return (
    <Section title="Todos for it" aside={todos.length ? `${open} open of ${todos.length}` : undefined}>
      {q.isLoading ? (
        <div className="dw-dim">Loading…</div>
      ) : q.error ? (
        <div className="tone-text-red">Could not load them: {String(q.error)}</div>
      ) : todos.length === 0 ? (
        <div className="dw-note">No todo is tagged for this {what}. Tag one under “For” in the todo form.</div>
      ) : (
        <ul className="dw-list">
          {todos.map((t) => (
            <li key={t.id}>
              <EntityLink kind="todo" id={t.id}>{t.title}</EntityLink>
              <span className="dw-dim">{t.status.replaceAll("_", " ")}</span>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}
