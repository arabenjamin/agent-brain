/**
 * What the user does with an item an agent can work on (HBI plan Phase 13):
 * hand it to an agent (with the model and the prompt it is told), approve a
 * run that waits, and approve or send back a result. Used in the Feed row and
 * in the item drawer.
 */
import { useState } from "react";
import { approveItem, assignItem, outcomeText, sendBackItem, type RunOutcome } from "../../api/feed";
import { useProfiles } from "../../api/queries";
import { openEntity } from "../drawer/entities";
import type { Todo } from "../todos/model";
import ModelPicker from "../ui/ModelPicker";

const MAX_RUNS = 3;

function useAct() {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const act = async (f: () => Promise<RunOutcome>) => {
    setBusy(true);
    setMsg(null);
    try {
      const o = await f();
      setMsg({ ok: o.outcome !== "not_ready", text: outcomeText(o) });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };
  return { busy, msg, act };
}

function Msg({ msg }: { msg: { ok: boolean; text: string } | null }) {
  return msg ? <span className={msg.ok ? "tone-text-green" : "tone-text-red"} style={{ fontSize: 11 }}>{msg.text}</span> : null;
}

/** Pick an agent, a model and the prompt, then hand the item over. */
export function HandToAgent({ item, onDone }: { item: Todo; onDone?: () => void }) {
  const profiles = (useProfiles().data ?? []).filter((p) => p.spawnable);
  const [agent, setAgent] = useState(item.assignee?.startsWith("agent:") ? item.assignee.slice(6) : "");
  const [model, setModel] = useState(item.model ?? "");
  const [prompt, setPrompt] = useState(item.prompt ?? [item.title, item.description].filter(Boolean).join("\n\n"));
  const { busy, msg, act } = useAct();
  const chosen = profiles.find((p) => p.name === agent);
  return (
    <div className="item-hand">
      <label>
        <span className="wk-dim">Agent</span>
        <select className="ui-input auto" value={agent} onChange={(e) => setAgent(e.target.value)}>
          <option value="">choose an agent…</option>
          {profiles.map((p) => (
            <option key={p.name} value={p.name}>{p.name}{p.asks_first ? " · asks first" : " · runs on its own"}</option>
          ))}
        </select>
      </label>
      {chosen && (
        <div className={chosen.asks_first ? "tone-text-yellow" : "wk-dim"} style={{ fontSize: 11 }}>
          {chosen.asks_first ? `It asks before it runs: ${chosen.asks_first}.` : "It writes nothing outside the brain, so it starts when due."}
          {chosen.model_preference && ` Its own model: ${chosen.model_preference}.`}
        </div>
      )}
      <label>
        <span className="wk-dim">Model</span>
        <ModelPicker value={model} onChange={setModel} purpose="subagent"
          dataClass={chosen?.data_class === "public" ? "public" : "private"}
          defaultLabel={`agent default (${chosen?.model_preference || "the chat model"})`} className="ui-input auto" />
      </label>
      <label>
        <span className="wk-dim">What it is told (it sees nothing else)</span>
        <textarea className="ui-input" rows={4} value={prompt} onChange={(e) => setPrompt(e.target.value)} />
      </label>
      <div className="item-actions">
        <button type="button" className="btn" disabled={busy || !agent || !prompt.trim()}
          onClick={() => act(async () => { const o = await assignItem(item.id, `agent:${agent}`, prompt, model); onDone?.(); return o; })}>
          {item.due_at ? "Hand over (runs when due)" : "Hand over"}
        </button>
        <Msg msg={msg} />
      </div>
    </div>
  );
}

/** Approve a run, approve or send back a result, or take the item back. */
export function ReviewActions({ item, compact }: { item: Todo; compact?: boolean }) {
  const [comment, setComment] = useState("");
  const [sending, setSending] = useState(false);
  const { busy, msg, act } = useAct();
  const agent = item.assignee?.startsWith("agent:") ? item.assignee.slice(6) : null;
  const runs = item.dispatch_count ?? 0;
  if (item.status === "awaiting_approval") {
    return (
      <span className="item-actions">
        <button type="button" className="btn" disabled={busy} onClick={() => act(() => approveItem(item.id))}
          title={`Start run ${runs + 1} of ${MAX_RUNS} with ${agent}; this approval is for this run only`}>Approve run</button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={() => act(() => assignItem(item.id, "user"))}>Take it back</button>
        <Msg msg={msg} />
      </span>
    );
  }
  if (item.status !== "awaiting_review") return null;
  return (
    <span className="item-actions" style={compact ? undefined : { flexDirection: "column", alignItems: "stretch" }}>
      <span className="item-actions">
        {item.result_note_id && (
          <button type="button" className="btn-ghost" onClick={() => openEntity("note", item.result_note_id!)}>Result</button>
        )}
        <button type="button" className="btn" disabled={busy} onClick={() => act(() => approveItem(item.id))}>Approve</button>
        {agent && runs < MAX_RUNS && (
          <button type="button" className="btn-ghost" disabled={busy} onClick={() => setSending((s) => !s)}>Send back</button>
        )}
        <Msg msg={msg} />
      </span>
      {sending && (
        <span className="item-actions">
          <input className="ui-input" placeholder={`What should ${agent} change? (run ${runs + 1} of ${MAX_RUNS})`} value={comment}
            onChange={(e) => setComment(e.target.value)} />
          <button type="button" className="btn" disabled={busy || !comment.trim()}
            onClick={() => act(async () => { const o = await sendBackItem(item.id, comment.trim()); setSending(false); setComment(""); return o; })}>
            Send
          </button>
        </span>
      )}
    </span>
  );
}
