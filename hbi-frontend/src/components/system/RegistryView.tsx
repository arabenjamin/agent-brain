/**
 * System → Registry: what the brain is made of, read-only. Skills and their
 * tools (with the existing tool explorer), agents (context profiles), workflows,
 * external MCP servers, providers, and the services it depends on. Edits go
 * through Settings; this is for looking.
 */
import { useState } from "react";
import { useRegistry, type Registry } from "../../api/system";
import { hrefTo } from "../../nav";
import ToolPanel from "../tools/ToolPanel";
import { EmptyState, Pill, Stat, Tabs } from "../ui";

type Tab = "tools" | "agents" | "workflows" | "mcp" | "providers" | "services";

export default function RegistryView() {
  const r = useRegistry();
  const [tab, setTab] = useState<Tab>("tools");
  const d = r.data;
  const c = d?.counts;
  return (
    <div className="sys reg">
      {c && (
        <div className="ui-stats reg-counts">
          <Stat label="Tools" value={c.tools} title={`${c.skills} skills`} onClick={() => setTab("tools")} />
          <Stat label="Dynamic tools" value={c.dynamic_tools} onClick={() => setTab("tools")} />
          <Stat label="Agents" value={c.agents} onClick={() => setTab("agents")} />
          <Stat label="Workflows" value={c.workflows} onClick={() => setTab("workflows")} />
          <Stat label="MCP servers" value={c.mcp_servers} onClick={() => setTab("mcp")} />
          <Stat label="Scheduled" value={`${c.schedules_enabled}/${c.schedules}`} title="scheduled workflows: on / all" href={hrefTo("work", { view: "scheduled" })} />
        </div>
      )}
      {r.error && <EmptyState tone="red" title="Couldn't load the registry" checked={r.error.message} />}
      <Tabs
        tabs={[
          { id: "tools", label: "Tools" },
          { id: "agents", label: "Agents" },
          { id: "workflows", label: "Workflows" },
          { id: "mcp", label: "MCP servers" },
          { id: "providers", label: "Providers" },
          { id: "services", label: "Services" },
        ]}
        active={tab}
        onChange={setTab}
      />
      <div className="reg-body scroll">
        {tab === "tools" && <ToolPanel />}
        {tab !== "tools" && !d && !r.error && <div className="loading">Loading the registry…</div>}
        {tab !== "tools" && d && <Section tab={tab} d={d} />}
      </div>
    </div>
  );
}

function Section({ tab, d }: { tab: Exclude<Tab, "tools">; d: Registry }) {
  if (tab === "agents") {
    return (
      <table className="reg-table">
        <thead><tr><th>Agent</th><th>Tools</th><th>Model</th><th>Data</th><th>What it does</th></tr></thead>
        <tbody>
          {d.agents.map((a) => (
            <tr key={a.name}>
              <td><strong>{a.name}</strong>{a.spawnable && <Pill tone="accent">sub-agent</Pill>}</td>
              <td>{a.tool_count}</td>
              <td className="wk-dim">{a.model_preference ?? "—"}{a.provider_hint ? ` · ${a.provider_hint}` : ""}</td>
              <td><Pill tone={a.data_class === "public" ? "muted" : "purple"}>{a.data_class ?? "private"}</Pill></td>
              <td className="reg-desc">{a.description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  if (tab === "workflows") {
    return d.workflows.length === 0 ? <EmptyState title="No workflows" checked="No (:Workflow) nodes; they are seeded from each plugin bundle's workflows/*.yaml." /> : (
      <table className="reg-table">
        <thead><tr><th>Workflow</th><th>Runs</th><th>Inputs</th><th>Steps</th><th>What it does</th></tr></thead>
        <tbody>
          {d.workflows.map((w) => (
            <tr key={w.id}>
              <td>
                <a href={hrefTo("work", { view: "scheduled", schedule: w.id })}><strong>{w.name}</strong></a>
                {w.default && <Pill tone="accent">default</Pill>}
                {!w.enabled && <Pill tone="muted">off</Pill>}
                {w.checks.map((c) => <Pill key={c} tone="muted">{c}</Pill>)}
              </td>
              <td className="wk-dim">{w.every ? `every ${w.every}` : "by name"}</td>
              <td className="wk-dim">{w.inputs.join(", ") || "—"}</td>
              <td>{w.steps}</td>
              <td className="reg-desc">{w.description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  if (tab === "mcp") {
    return d.mcp_servers.length === 0 ? <EmptyState title="No external MCP servers" checked="No (:McpServer) nodes; they are seeded from mcp-servers/*.yaml." /> : (
      <table className="reg-table">
        <thead><tr><th>Server</th><th>Endpoint</th><th>Auth</th><th>What it offers</th></tr></thead>
        <tbody>
          {d.mcp_servers.map((m) => (
            <tr key={m.name}>
              <td><strong>{m.name}</strong> {!m.enabled && <Pill tone="muted">disabled</Pill>}</td>
              <td className="wk-dim">{m.endpoint}</td>
              <td>
                {!m.auth_env_var ? <span className="wk-dim">none</span>
                  : m.auth_configured ? <Pill tone="green">{m.auth_env_var} set</Pill>
                  : <Pill tone="yellow">{m.auth_env_var} missing</Pill>}
              </td>
              <td className="reg-desc">{m.description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  if (tab === "providers") {
    /* eslint-disable-next-line @typescript-eslint/no-explicit-any -- provider_registry's own JSON */
    const list: any[] = Array.isArray(d.providers) ? d.providers : d.providers?.providers ?? [];
    return (
      <>
        <table className="reg-table">
          <thead><tr><th>Provider</th><th>Kind</th><th>Billing</th><th>Data policy</th><th>Key</th><th>Spend cap</th></tr></thead>
          <tbody>
            {list.map((p) => (
              <tr key={p.name ?? p.kind}>
                <td><strong>{p.name ?? p.kind}</strong> {!p.enabled && <Pill tone="muted">off</Pill>}</td>
                <td className="wk-dim">{p.kind}</td>
                <td>{p.billing}</td>
                <td>{p.data_policy}</td>
                <td>{p.key_configured === false ? <Pill tone="yellow">{p.api_key_env ?? "key"} missing</Pill> : p.api_key_env ? <Pill tone="green">set</Pill> : <span className="wk-dim">none needed</span>}</td>
                <td className="wk-dim">{p.spend ? `$${p.spend.daily_usd ?? "?"}/day · $${p.spend.monthly_usd ?? "?"}/month` : "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <div className="wk-dim reg-note">Models and the routing policy are in <a href={hrefTo("settings", { section: "models" })}>Settings → Models</a>.</div>
      </>
    );
  }
  return (
    <table className="reg-table">
      <thead><tr><th>Service</th><th>Kind</th><th>Needed</th><th>What it is for</th></tr></thead>
      <tbody>
        {d.components.map((c) => (
          <tr key={c.name}>
            <td><strong>{c.label}</strong></td>
            <td className="wk-dim">{c.kind}</td>
            <td>{c.required ? <Pill tone="red">required</Pill> : <span className="wk-dim">optional</span>}</td>
            <td className="reg-desc">{c.purpose}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
