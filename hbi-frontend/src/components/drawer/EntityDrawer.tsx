/**
 * The one inspection surface. Any workspace opens an entity by putting it in
 * the URL (`openEntity("note", id)` → `#/<workspace>?note=<id>`), so the
 * drawer survives reloads and every open entity is a shareable link.
 *
 * Kinds: note (Phase 0, enriched in Phase 3); task, job, schedule, todo
 * (Phase 2); entity (Phase 3); topic (Phase 4b). A proposal is a todo.
 */
import { useRoute } from "../../router";
import { drawerTarget } from "./entities";
import EntityView from "./EntityView";
import JobView from "./JobView";
import NoteView from "./NoteView";
import ScheduleView from "./ScheduleView";
import TaskView from "./TaskView";
import TodoView from "./TodoView";
import TopicView from "./TopicView";

export default function EntityDrawer() {
  const route = useRoute();
  const target = drawerTarget(route.params);
  if (!target) return null;
  // Keyed by id, so moving from one entity to another resets view state
  // (an open editor, an action's result).
  switch (target.kind) {
    case "note":
      return <NoteView key={target.id} id={target.id} />;
    case "entity":
      return <EntityView key={target.id} id={target.id} />;
    case "topic":
      return <TopicView key={target.id} name={target.id} />;
    case "task":
      return <TaskView key={target.id} id={target.id} />;
    case "job":
      return <JobView key={target.id} id={target.id} />;
    case "schedule":
      return <ScheduleView key={target.id} id={target.id} />;
    case "todo":
      return <TodoView key={target.id} id={target.id} />;
  }
}
