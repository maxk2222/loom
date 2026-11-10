import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../../lib/api";
import { WindowPicker } from "../Overview";
import { AreaChart, Area, BarChart, Bar, ComposedChart, ResponsiveContainer, XAxis, YAxis, Tooltip, CartesianGrid, Legend } from "recharts";
import { Badge, Card, Stat } from "../../components/ui";
import { Megaphone, X } from "lucide-react";
import type { ClientNote, ClientStatus } from "../../lib/types";
import { fmtPct, fmtTs, fmtTimeS, fmtDayT, fmtUptime, fmtGiB, fmtNum, fmtMs } from "../../lib/format";

function Meter({ label, pct, limit }: { label: string; pct: number; limit: number }) {
  const over = limit > 0 && pct >= limit;
  return (
    <div>
      <div className="mb-1 flex justify-between text-xs">
        <span className="text-muted">{label}</span>
        <span className={over ? "text-err" : "text-text"}>{fmtPct(pct)}%{limit > 0 && <span className="text-muted"> / limit {limit}%</span>}</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-border">
        <div className={`h-full rounded-full transition-all ${over ? "bg-err" : pct > 80 ? "bg-warn" : "bg-ok"}`} style={{ width: `${Math.min(100, pct)}%` }} />
      </div>
    </div>
  );
}

const SEV_TONE: Record<string, "muted" | "warn" | "err"> = { info: "muted", warning: "warn", critical: "err" };

function NotesCard() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["client-notes"],
    queryFn: () => api.get<{ notes: ClientNote[]; undismissed: number }>("/local/api/notes"),
    refetchInterval: 8000,
  });
  const notes = (q.data?.notes ?? []).filter(n => !n.dismissed);
  if (notes.length === 0) return null;
  const dismiss = async (id: string) => {
    await api.post(`/local/api/notes/${id}/dismiss`, {});
    qc.invalidateQueries({ queryKey: ["client-notes"] });
  };
  return (
    <Card className="space-y-3">
      <div className="flex items-center gap-2 text-sm font-medium">
        <Megaphone className="h-4 w-4 text-accent" /> Notices
        {(q.data?.undismissed ?? 0) > 0 && <Badge tone="warn">{q.data?.undismissed} new</Badge>}
      </div>
      {notes.map(n => (
        <div key={n.id} className={`rounded-md border p-3 ${n.severity === "critical" ? "border-err/40 bg-err/5" : n.severity === "warning" ? "border-warn/40 bg-warn/5" : "border-border bg-card"}`}>
          <div className="flex items-start justify-between gap-2">
            <div className="text-sm font-medium">{n.title}</div>
            <div className="flex shrink-0 items-center gap-1.5">
              <Badge tone={SEV_TONE[n.severity] ?? "muted"}>{n.severity}</Badge>
              {n.persistent && <Badge tone="muted">pinned</Badge>}
              {!n.persistent && (
                <button className="rounded p-0.5 text-muted hover:text-text" onClick={() => dismiss(n.id)} title="dismiss">
                  <X className="h-3.5 w-3.5" />
                </button>
              )}
            </div>
          </div>
          {n.body && <div className="mt-1 whitespace-pre-wrap text-xs text-muted">{n.body}</div>}
          <div className="mt-1 text-[10px] text-muted/60">{fmtTs(n.created_at_unix * 1000)}</div>
        </div>
      ))}
    </Card>
  );
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
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4 md:grid-cols-4">
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
      {d?.system && (
        <Card className="space-y-3">
          <div className="flex items-center justify-between">
            <div className="text-xs font-medium uppercase tracking-wide text-muted">System load</div>
            {d.system.pressure ? (
              <Badge tone="err">pressure — tier-2 paused</Badge>
            ) : d.system.cooldown ? (
              <Badge tone="warn">cooldown — reopening soon</Badge>
            ) : d.system.enabled ? (
              <Badge tone="ok">within limits</Badge>
            ) : (
              <Badge tone="muted">monitor off</Badge>
            )}
          </div>
          <Meter label="CPU" pct={d.system.cpu_pct} limit={d.system.cpu_limit_pct} />
          <Meter label={`RAM (${fmtGiB(d.system.ram_used_bytes)} / ${fmtGiB(d.system.ram_total_bytes)} GB)`} pct={d.system.ram_pct} limit={d.system.ram_limit_pct} />
          {d.system.gpu && (
            <>
              {d.system.gpu.util_pct != null && (
                <Meter label={`GPU · ${d.system.gpu.name}`} pct={d.system.gpu.util_pct} limit={0} />
              )}
              {d.system.gpu.mem_total_bytes != null && d.system.gpu.mem_total_bytes > 0 && (
                <Meter
                  label={`VRAM (${fmtGiB(d.system.gpu.mem_used_bytes ?? 0)} / ${fmtGiB(d.system.gpu.mem_total_bytes)} GB)`}
                  pct={(d.system.gpu.mem_used_bytes ?? 0) / d.system.gpu.mem_total_bytes * 100}
                  limit={d.system.gpu_vram_limit_pct ?? 0}
                />
              )}
            </>
          )}
          <div className="text-[10px] text-muted">When a limit is exceeded, tier-2 gates close until load stays below the limit for the cooldown window.</div>
        </Card>
      )}
      <NotesCard />
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
      <Card className="overflow-x-auto p-0">
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
                <td className="td">{c.accepting ? <Badge tone="ok">yes</Badge> : c.pressure_gated ? <Badge tone="err">pressure</Badge> : <Badge tone="warn">no</Badge>}</td>
              </tr>
            ))}
            {(d?.capacities ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No deployments configured.</td></tr>}
          </tbody>
        </table>
      </Card>
    </div>

            );
}


const CLIENT_WINDOWS = [
  { v: "1h", l: "1h" },
  { v: "6h", l: "6h" },
  { v: "24h", l: "24h" },
  { v: "all", l: "All" },
];
const WIN_MS: Record<string, number> = { "1h": 3.6e6, "6h": 21.6e6, "24h": 86.4e6, all: Infinity };

function ClientCharts() {
  const q = useQuery({ queryKey: ["client-hist"], queryFn: () => api.get<{ history: any[] }>("/local/api/history"), refetchInterval: 8000 });
  const bk = useQuery({ queryKey: ["chart-backends"], queryFn: () => api.get<{ backends: LocalBackend[] }>("/local/api/backends"), refetchInterval: 30000 });
  const [win, setWin] = useState("24h");
  // History is a ~300-entry ring — filter client-side by the picked window.
  const rows = (q.data?.history ?? []).filter(r => Date.now() - r.ended_unix_ms <= WIN_MS[win]);
  const modelName = (() => {
    const m = new Map<string, string>();
    for (const b of bk.data?.backends ?? []) for (const d of b.deployments ?? []) m.set(d.id, d.model_name || d.name);
    return m;
  })();
  const byModel = (() => {
    const m = new Map<string, { name: string; t1: number; t2: number; pt: number; ct: number; dur: number; decdur: number }>();
    for (const r of rows) {
      const name = modelName.get(r.deployment_id) ?? (r.deployment_id ? String(r.deployment_id).slice(0, 8) : "?");
      const b = m.get(name) ?? { name, t1: 0, t2: 0, pt: 0, ct: 0, dur: 0, decdur: 0 };
      if (r.class === "local") b.t1++; else b.t2++;
      b.pt += r.prompt_tokens ?? 0; b.ct += r.completion_tokens ?? 0;
      b.dur += (r.duration_ms ?? 0) / 1000;
      b.decdur += Math.max(0, (r.duration_ms ?? 0) - (r.ttft_ms ?? 0)) / 1000;
      m.set(name, b);
    }
    return [...m.values()].map(b => ({
      ...b,
      e2e: b.dur > 0 ? +(((b.pt + b.ct) / b.dur).toFixed(1)) : 0,
      dec: b.decdur > 0 ? +((b.ct / b.decdur).toFixed(1)) : 0,
    }));
  })();
  const byClass = (() => {
    const m = new Map<string, number>();
    for (const r of rows) m.set(r.class, (m.get(r.class) ?? 0) + 1);
    return [...m.entries()].map(([name, n]) => ({ name: name === "local" ? "tier-1 local" : "tier-2 shared", n }));
  })();
  // Time-bucketed requests + total tokens — mirrors the server's requests
  // chart. Window "all" derives its span from the oldest sample.
  const reqSeries = (() => {
    if (rows.length === 0) return [];
    const span = Number.isFinite(WIN_MS[win])
      ? WIN_MS[win]
      : Date.now() - rows[0].ended_unix_ms;
    const end = Date.now();
    const N = Math.min(30, Math.max(6, Math.floor(rows.length / 3)));
    const bw = span / N;
    const tick = win === "all" || win === "24h" ? fmtDayT : fmtTimeS;
    const bkt = Array.from({ length: N }, (_, i) => ({
      t: tick(end - span + (i + 0.5) * bw), n: 0, tok: 0,
    }));
    for (const r of rows) {
      const i = Math.min(N - 1, Math.max(0, Math.floor((r.ended_unix_ms - (end - span)) / bw)));
      bkt[i].n++;
      bkt[i].tok += (r.prompt_tokens ?? 0) + (r.completion_tokens ?? 0);
    }
    return bkt;
  })();
  const latChart = rows.slice(-30).map((r: any) => ({ t: fmtTimeS(r.ended_unix_ms), ms: r.duration_ms }));
  const tokRows = rows.slice(-30).map((r: any) => ({
    t: fmtTimeS(r.ended_unix_ms),
    pt: r.prompt_tokens ?? 0,
    ct: r.completion_tokens ?? 0,
    e2e: r.duration_ms > 0 ? +(((r.prompt_tokens ?? 0) + (r.completion_tokens ?? 0)) / (r.duration_ms / 1000)).toFixed(1) : 0,
    dec: r.duration_ms > (r.ttft_ms ?? 0) && (r.ttft_ms ?? 0) > 0 ? +((r.completion_tokens ?? 0) / ((r.duration_ms - r.ttft_ms) / 1000)).toFixed(1) : 0,
    cls: r.class === "local" ? "t1" : "t2",
  }));
  if (rows.length === 0) return null;
  return (
    <div className="space-y-4">
    <div className="flex items-center justify-between">
      <div className="text-xs font-medium uppercase tracking-wide text-muted">Request analytics</div>
      <WindowPicker value={win} onChange={setWin} options={CLIENT_WINDOWS} />
    </div>
    {reqSeries.length > 0 && (
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests over time</div>
        <ResponsiveContainer width="100%" height={150}>
          <ComposedChart data={reqSeries} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis yAxisId="l" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} tickFormatter={fmtNum} />
            <YAxis yAxisId="r" orientation="right" tick={{ fontSize: 10, fill: "#8fb3d9" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
            <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Area yAxisId="l" type="monotone" dataKey="n" name="requests" stroke="#3d6fae" fill="#3d6fae" fillOpacity={0.16} strokeWidth={2} isAnimationActive={false} />
            <Area yAxisId="r" type="monotone" dataKey="tok" name="tokens" stroke="#8fb3d9" fill="none" strokeWidth={1.5} dot={false} isAnimationActive={false} />
          </ComposedChart>
        </ResponsiveContainer>
      </div>
    )}
    <div className="grid gap-4 md:grid-cols-2">
      {byClass.length > 0 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests by class</div>
          <ResponsiveContainer width="100%" height={130}>
            <BarChart data={byClass} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} tickFormatter={fmtNum} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
              <Bar dataKey="n" fill="#3d6fae" radius={[4, 4, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">latency</div>
        <ResponsiveContainer width="100%" height={130}>
          <AreaChart data={latChart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={(v) => fmtMs(Number(v))} />
            <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtMs(Number(v))} />
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
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
            <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Bar dataKey="pt" name="prompt" stackId="tok" fill="#3d6fae" />
            <Bar dataKey="ct" name="completion" stackId="tok" fill="#7fa8d9" radius={[4, 4, 0, 0]} />
          </BarChart>
        </ResponsiveContainer>
      </div>
      {byModel.length > 0 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests / model <span className="normal-case text-muted/70">(tier-1 + tier-2)</span></div>
          <ResponsiveContainer width="100%" height={130}>
            <BarChart data={byModel} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} tickFormatter={fmtNum} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
              <Legend wrapperStyle={{ fontSize: 11 }} />
              <Bar dataKey="t1" name="tier-1 local" stackId="c" fill="#3d6fae" />
              <Bar dataKey="t2" name="tier-2 shared" stackId="c" fill="#7fa8d9" radius={[4, 4, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
      {byModel.length > 0 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tok/s / model <span className="normal-case text-muted/70">(e2e vs decode)</span></div>
          <ResponsiveContainer width="100%" height={130}>
            <BarChart data={byModel} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
              <Legend wrapperStyle={{ fontSize: 11 }} />
              <Bar dataKey="e2e" name="e2e tok/s" fill="#3d6fae" radius={[4, 4, 0, 0]} />
              <Bar dataKey="dec" name="decode tok/s" fill="#8fb3d9" radius={[4, 4, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="card p-4">
        <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tok/s <span className="normal-case text-muted/70">(e2e vs decode)</span></div>
        <ResponsiveContainer width="100%" height={130}>
          <ComposedChart data={tokRows} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis dataKey="t" tick={{ fontSize: 9, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
            <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
            <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Area type="monotone" dataKey="e2e" name="e2e tok/s" stroke="#3d6fae" fill="#3d6fae" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
            <Area type="monotone" dataKey="dec" name="decode tok/s" stroke="#8fb3d9" fill="none" strokeWidth={1.5} dot={false} isAnimationActive={false} />
          </ComposedChart>
        </ResponsiveContainer>
      </div>
    </div>
    {byModel.length > 0 && (
      <div className="card overflow-x-auto p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Token totals by model <span className="text-xs text-muted">({win})</span></div>
        <table className="w-full">
          <thead><tr>
            <th className="th">Model</th>
            <th className="th text-right">tier-1</th>
            <th className="th text-right">tier-2</th>
            <th className="th text-right">Prompt</th>
            <th className="th text-right">Completion</th>
            <th className="th text-right">Total</th>
          </tr></thead>
          <tbody>
            {byModel.map(m => (
              <tr key={m.name}>
                <td className="td">{m.name}</td>
                <td className="td text-right text-muted">{fmtNum(m.t1)}</td>
                <td className="td text-right text-muted">{fmtNum(m.t2)}</td>
                <td className="td text-right text-muted">{fmtNum(m.pt)}</td>
                <td className="td text-right text-muted">{fmtNum(m.ct)}</td>
                <td className="td text-right font-medium">{fmtNum(m.pt + m.ct)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    )}
    </div>
  );
}
