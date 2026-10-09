/**
 * "Make it a schedule" (HBI plan Phase 13): `schedule-author` drafts a
 * ScheduledTask from the item, read-only; it is created DISABLED, linked
 * `(:Todo)-[:BECAME]->(:ScheduledTask)`, and opened in the schedule drawer for
 * review. Turning it on closes the item as "became a schedule". An item never
 * recurs itself; recurring work is a schedule.
 */
import { useState } from "react";
import { apiRequest } from "../../api/client";
import { invalidateWork } from "../../api/work";
import { answerBody, extractJson, runAssist } from "../../lib/assist";
import { openEntity } from "../drawer/entities";
import { useProfiles } from "../../api/queries";
import { useMergedToolGroups } from "../schedules/useToolGroups";
import ModelPicker from "../ui/ModelPicker";
import type { Todo } from "../todos/model";

export default function MakeSchedule({ item }: { item: Todo }) {
  const toolGroups = useMergedToolGroups();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // schedule-author's own model is the default; a small local model can miss
  // the step rules, and the save rejects what it gets wrong.
  const [model, setModel] = useState("");
  const own = useProfiles().data?.find((p) => p.name === "schedule-author")?.model_preference;
  const prompt = item.prompt?.trim() || [item.title, item.description].filter(Boolean).join("\n\n");

  /** One draft from schedule-author; `fix` names what a previous draft got wrong. */
  const draft = async (fix?: { previous: string; errors: string }) => {
    let task =
      `Draft a scheduled task that does this regularly:\n"${prompt}"\n\n` +
      `Tools available to a scheduled step: ${toolGroups.flatMap((g) => g.tools).join(", ")}`;
    if (fix) {
      task += `\n\nYour previous draft was rejected when saved:\n${fix.errors}\n\nThe draft was:\n${fix.previous}\n\nReturn the corrected object.`;
    }
    const run = await runAssist("schedule-author", task, model);
    const text = extractJson(answerBody(run.answer), "object");
    if (!text) throw new Error(`No schedule in schedule-author's reply:\n${answerBody(run.answer).slice(0, 400)}`);
    const d = JSON.parse(text) as { name?: string; interval_seconds?: number; steps?: unknown[] };
    if (!Array.isArray(d.steps) || d.steps.length === 0) throw new Error("The draft has no steps");
    return { d, text };
  };

  const save = async (d: { name?: string; interval_seconds?: number; steps?: unknown[] }) => {
    const res = await apiRequest("/scheduled-tasks", {
      method: "POST",
      body: JSON.stringify({
        name: d.name?.trim() || item.title,
        description: `Drafted from the item “${item.title}”. Review the steps, then turn it on; that closes the item.`,
        enabled: false,
        interval_seconds: typeof d.interval_seconds === "number" && d.interval_seconds >= 60 ? d.interval_seconds : 86400,
        steps: d.steps,
        from_item: item.id,
      }),
    });
    const saved = (await res.json().catch(() => ({}))) as { id?: string; error?: string };
    return { ok: res.ok && !!saved.id, status: res.status, id: saved.id, error: saved.error ?? `HTTP ${res.status}` };
  };

  const make = async () => {
    setBusy(true);
    setError(null);
    try {
      let { d, text } = await draft();
      let saved = await save(d);
      // A draft the save path rejects (a wrong argument, an unknown tool) goes
      // back to schedule-author once, with the errors, before giving up.
      if (!saved.ok && saved.status === 400) {
        ({ d, text } = await draft({ previous: text, errors: saved.error }));
        saved = await save(d);
      }
      if (!saved.ok || !saved.id) throw new Error(saved.error);
      await invalidateWork();
      openEntity("schedule", saved.id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="item-actions" style={{ flexDirection: "column", alignItems: "flex-start" }}>
      <span className="item-actions">
        <button type="button" className="btn-ghost" disabled={busy} onClick={make}
          title="schedule-author drafts a schedule from this item; it is created turned off for you to review">
          {busy ? "Drafting a schedule…" : "Make it a schedule"}
        </button>
        <ModelPicker value={model} onChange={setModel} purpose="subagent" className="ui-input auto"
          defaultLabel={`drafted on schedule-author's own (${own || "the chat model"})`} />
      </span>
      {error && <span className="tone-text-red" style={{ fontSize: 11, whiteSpace: "pre-wrap" }}>{error}</span>}
    </div>
  );
}
