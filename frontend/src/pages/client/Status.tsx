import { useQuery } from "@tanstack/react-query";
import { api } from "../../lib/api";
import { AreaChart, Area, BarChart, Bar, ComposedChart, ResponsiveContainer, XAxis, YAxis, Tooltip, CartesianGrid, Legend } from "recharts";
import { Badge, Card, Stat } from "../../components/ui";
import type { ClientStatus } from "../../lib/types";

function fmtUptime(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
}

const LOCAL_ENDPOINTS = [
  { m: "POST", p: "/v1/chat/completions", d: "chat · stream · tools" },
  { m: "POST", p: "/v1/completions", d: "legacy completions" },
  { m: "POST", p: "/v1/embeddings", d: "embeddings" },
  { m: "GET", p: "/v1/models", d: "OpenAI model list" },
];

interface LocalBackend { id: string; name: string; backend_type: string; deployments: { id: string; name: string; model_name: string; enabled: boolean }[] }

export default function ClientStatusPage() {
  const q = useQuery({
    queryKey: ["client-status"],
    queryFn: () => api.get<ClientStatus>("/local/api/status"),
    refetchInterval: 3000,
  });
  const backends = useQuery({
    queryKey: ["client-backends"],
    queryFn: () => api.get<{ backends: LocalBackend[] }>("/local/api/backends"),
    refetchInterval: 8000,
  });
  const models = (backends.data?.backends ?? []).flatMap(b =>
    (b.deployments ?? []).filter(x => x.enabled).map(x => ({ ...x, backend: b.name }))
  );
  const d = q.data;
  const totalFree = (d?.capacities ?? []).reduce((a, c) => a + c.tier2_free, 0);
  const totalCap = (d?.capacities ?? []).reduce((a, c) => a + c.tier2_total, 0);

  return (
    <div className="space-y-6">
      <h1 className="text-lg font-semibold">Status</h1>
      <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat label="Server connection" value={<Badge tone={d?.server_connected ? "ok" : "err"}>{d?.server_connected ? "connected" : "offline"}</Badge>} />
        <Stat label="Uptime" value={fmtUptime(d?.uptime_secs ?? 0)} />
        <Stat label="Sharing" value={<Badge tone={d?.sharing_enabled ? "ok" : "muted"}>{d?.sharing_enabled ? "enabled" : "off"}</Badge>} />
        <Stat label="Tier-2 slots free" value={`${totalFree}/${totalCap}`} />
      </div>
      <Card className="space-y-2 text-sm">
        <div className="flex justify-between"><span className="text-muted">Client name</span><span>{d?.name ?? "—"}</span></div>
        <div className="flex justify-between"><span className="text-muted">Version</span><span>{d?.version ?? "—"}</span></div>
        <div className="flex justify-between"><span className="text-muted">Server</span><span className="font-mono text-xs">{d?.server_url || "not configured"}</span></div>
      </Card>
      <div className="grid gap-4 md:grid-cols-2">
        <Card className="space-y-1.5">
          <div className="mb-1 text-xs font-medium uppercase tracking-wide text-muted">Local API <span className="normal-case text-muted/70">(port 8391)</span></div>
          {LOCAL_ENDPOINTS.map(e => (
            <div key={e.p} className="flex items-center gap-2.5 text-xs">
              <span className={`w-10 rounded px-1.5 py-0.5 text-center font-mono text-[10px] ${e.m === "GET" ? "bg-ok/10 text-ok" : "bg-accent/10 text-accent"}`}>{e.m}</span>
              <code className="font-mono text-text">{e.p}</code>
              <span className="ml-auto text-[10px] text-muted">{e.d}</span>
            </div>
          ))}
        </Card>
        <Card className="space-y-1.5">
          <div className="mb-1 text-xs font-medium uppercase tracking-wide text-muted">Available models</div>
          {models.length === 0 && <div className="text-xs text-muted">No enabled deployments.</div>}
          {models.map(m => (
            <div key={m.id} className="flex items-center gap-2.5 text-xs">
              <span className="h-1.5 w-1.5 rounded-full bg-ok" />
              <code className="truncate font-mono text-text">{m.model_name}</code>
              <span className="ml-auto shrink-0 text-[10px] text-muted">{m.backend}</span>
            </div>
          ))}
        </Card>
      </div>
      <ClientCharts />
      <Card className="overflow-hidden p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Per-deployment capacity</div>
        <table className="w-full">
          <thead><tr><th className="th">Deployment</th><th className="th">Mode</th><th className="th">Tier-2</th><th className="th">Local inflight</th><th className="th">Tier-2 inflight</th><th className="th">Accepting</th></tr></thead>
          <tbody>
            {(d?.capacities ?? []).map((c) => (
              <tr key={c.deployment_id}>
                <td className="td font-mono text-xs">{c.deployment_id.slice(0, 8)}…</td>
                <td className="td text-muted">{c.sharing_mode}</td>
                <td className="td">{c.tier2_free}/{c.tier2_total}</td>
                <td className="td text-muted">{c.inflight_local}</td>
                <td className="td text-muted">{c.inflight_tier2}</td>
                <td className="td">{c.accepting ? <Badge tone="ok">yes</Badge> : <Badge tone="warn">no</Badge>}</td>
              </tr>
            ))}
            {(d?.capacities ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No deployments configured.</td></tr>}
          </tbody>
        </table>
      </Card>
    </div>

            );
}


function ClientCharts() {
  const q = useQuery({ queryKey: ["client-hist"], queryFn: () => api.get<{ history: any[] }>("/local/api/history"), refetchInterval: 8000 });
  const rows = q.data?.history ?? [];
  const byClass = (() => {
    const m = new Map<string, number>();
    for (const r of rows) m.set(r.class, (m.get(r.class) ?? 0) + 1);
    return [...m.entries()].map(([name, n]) => ({ name: name === "local" ? "tier-1 local" : "tier-2 shared", n }));
  })();
  const latChart = rows.slice(-30).map((r: any) => ({ t: new Date(r.ended_unix_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }), ms: r.duration_ms }));
  const tokRows = rows.slice(-30).map((r: any) => ({
    t: new Date(r.ended_unix_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }),
    pt: r.prompt_tokens ?? 0,
    ct: r.completion_tokens ?? 0,
    e2e: r.duration_ms > 0 ? +(((r.prompt_tokens ?? 0) + (r.completion_tokens ?? 0)) / (r.duration_ms / 1000)).toFixed(1) : 0,
    dec: r.duration_ms > (r.ttft_ms ?? 0) && (r.ttft_ms ?? 0) > 0 ? +((r.completion_tokens ?? 0) / ((r.duration_ms - r.ttft_ms) / 1000)).toFixed(1) : 0,
    cls: r.class === "local" ? "t1" : "t2",
  }));
  if (rows.length === 0) return null;
  return (
    <div className="grid gap-4 md:grid-cols-2">
      {byClass.length > 0 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests by class</div>
          <ResponsiveContainer width="100%" height={130}>
            <BarChart data={byClass} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} />
              <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
              <Bar dataKey="n" fill="#3d6fae" radius={[4, 4, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">latency (ms)</div>
        <ResponsiveContainer width="100%" height={130}>
          <AreaChart data={latChart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
            <Area type="monotone" dataKey="ms" stroke="#4a86c8" fill="#4a86c8" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
          </AreaChart>
        </ResponsiveContainer>
      </div>
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tokens / request</div>
        <ResponsiveContainer width="100%" height={130}>
          <BarChart data={tokRows} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 9, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Bar dataKey="pt" name="prompt" stackId="tok" fill="#3d6fae" />
            <Bar dataKey="ct" name="completion" stackId="tok" fill="#7fa8d9" radius={[4, 4, 0, 0]} />
          </BarChart>
        </ResponsiveContainer>
      </div>
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tok/s <span className="normal-case text-muted/70">(e2e vs decode)</span></div>
        <ResponsiveContainer width="100%" height={130}>
          <ComposedChart data={tokRows} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 9, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Area type="monotone" dataKey="e2e" name="e2e tok/s" stroke="#3d6fae" fill="#3d6fae" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
            <Area type="monotone" dataKey="dec" name="decode tok/s" stroke="#8fb3d9" fill="none" strokeWidth={1.5} dot={false} isAnimationActive={false} />
          </ComposedChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
