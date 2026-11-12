import { useQuery } from "@tanstack/react-query";
import { Activity, Boxes, Cpu, ListOrdered, Zap } from "lucide-react";
import { AreaChart, Area, BarChart, Bar, Cell, ComposedChart, ResponsiveContainer, XAxis, YAxis, Tooltip, CartesianGrid, Legend } from "recharts";
import { useEffect, useState } from "react";
import { api, openEventStream } from "../lib/api";
import { Badge, LiveBadge, PageHeader, Stat } from "../components/ui";
import type { ClientRow, Overview, VirtualModel } from "../lib/types";
import { fmtPct, fmtTs, fmtTime, fmtDay, fmtDayT, fmtNum, fmtMs, PRIORITY_LABELS, prioTone } from "../lib/format";

const API_ENDPOINTS = [
  { m: "POST", p: "/v1/chat/completions", d: "chat · stream · tools · vision" },
  { m: "POST", p: "/v1/completions", d: "legacy completions" },
  { m: "POST", p: "/v1/embeddings", d: "embeddings" },
  { m: "GET", p: "/v1/models", d: "OpenAI model list" },
];

const WINDOWS = [
  { v: "24h", l: "24h" },
  { v: "7d", l: "7d" },
  { v: "30d", l: "30d" },
  { v: "365d", l: "1y" },
  { v: "all", l: "All" },
];

interface SeriesBucket {
  t: string;
  requests: number;
  prompt_tokens: number;
  completion_tokens: number;
  ttft_ms: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  duration_ms: number;
  decode_ms: number;
}

interface BreakdownRow {
  name: string;
  extra: string | null;
  requests: number;
  prompt_tokens: number;
  completion_tokens: number;
  tokens: number;
  duration_ms: number;
  decode_ms: number;
}

export function WindowPicker({ value, onChange, options = WINDOWS }: { value: string; onChange: (v: string) => void; options?: { v: string; l: string }[] }) {
  return (
    <div className="flex gap-0.5 rounded-md bg-panel2 p-0.5">
      {options.map(w => (
        <button
          key={w.v}
          onClick={() => onChange(w.v)}
          className={`rounded px-2 py-0.5 text-[11px] transition-colors ${value === w.v ? "bg-accent/20 text-accent" : "text-muted hover:text-text"}`}
        >{w.l}</button>
      ))}
    </div>
  );
}

export default function OverviewPage() {
  const [win, setWin] = useState("24h");
  const ov = useQuery({ queryKey: ["overview"], queryFn: () => api.get<Overview>("/api/overview"), refetchInterval: 5000 });
  const clients = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const series = useQuery({
    queryKey: ["stats-series", win],
    queryFn: () => api.get<{ buckets: SeriesBucket[] }>(`/api/stats/series?window=${win}`),
    refetchInterval: 15000,
  });
  const brk = useQuery({
    queryKey: ["stats-brk", win],
    queryFn: () => api.get<{ models: BreakdownRow[]; clients: BreakdownRow[]; keys: BreakdownRow[]; projects: BreakdownRow[] }>(`/api/stats/breakdown?window=${win}`),
    refetchInterval: 15000,
  });
  // Keys must match the owning pages ("deployments"/"vms") — a divergent
  // key forks the cache and leaves this view permanently stale.
  const depCap = useQuery({ queryKey: ["deployments"], queryFn: () => api.get<{ deployments: any[] }>("/api/deployments"), refetchInterval: 5000 });
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: VirtualModel[] }>("/api/models"), refetchInterval: 15000 });

  // Tick formatter per window: HH:mm for a day, "MM-DD HH:mm" for week/month,
  // "YYYY-MM-DD" beyond that.
  const tickFmt = win === "24h" ? fmtTime : win === "7d" || win === "30d" ? fmtDayT : fmtDay;
  const buckets = series.data?.buckets ?? [];
  const chart = buckets.map(b => ({ t: tickFmt(b.t), n: b.requests, tok: b.prompt_tokens + b.completion_tokens }));
  const ttftChart = buckets.filter(b => b.ttft_ms > 0).map(b => ({ t: tickFmt(b.t), ttft: Math.round(b.ttft_ms) }));
  const latChart = buckets.filter(b => b.p50_ms > 0).map(b => ({ t: tickFmt(b.t), p50: Math.round(b.p50_ms), p95: Math.round(b.p95_ms), p99: Math.round(b.p99_ms) }));
  const tokSeries = buckets.map(b => ({
    t: tickFmt(b.t), pt: b.prompt_tokens, ct: b.completion_tokens,
    tps: b.duration_ms > 0 ? +(((b.prompt_tokens + b.completion_tokens) / (b.duration_ms / 1000)).toFixed(1)) : 0,
  }));
  const reqPerClient = (brk.data?.clients ?? []).map(c => ({ name: c.name, n: c.requests }));
  const tpsOf = (r: BreakdownRow) => ({
    name: r.name,
    e2e: r.duration_ms > 0 ? +((r.tokens / (r.duration_ms / 1000)).toFixed(1)) : 0,
    dec: r.decode_ms > 0 ? +((r.completion_tokens / (r.decode_ms / 1000)).toFixed(1)) : 0,
  });
  const tpsModel = (brk.data?.models ?? []).map(tpsOf);
  const tpsClient = (brk.data?.clients ?? []).map(tpsOf);
  const perClient = (() => {
    const m = new Map<string, { name: string; cap: number }>();
    for (const d of depCap.data?.deployments ?? []) {
      const n = clients.data?.clients.find(c => c.id === d.client_id)?.name ?? "client";
      const b = m.get(n) ?? { name: n, cap: 0 };
      b.cap += d.runtime?.tier2_total ?? 0; m.set(n, b);
    }
    return [...m.values()];
  })();
  const [, setTick] = useState(0);

  useEffect(() => {
    const stop = openEventStream(() => {
      setTick((t) => t + 1);
      ov.refetch();
      clients.refetch();
    });
    return stop;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const d = ov.data;
  const online = (clients.data?.clients ?? []).filter((c) => c.online);
  // Fleet view shows every client — suspended/disconnected ones stay
  // visible but grayed out instead of disappearing silently.
  const fleet = [...(clients.data?.clients ?? [])].sort(
    (a, b) => Number(b.online && b.status === "active") - Number(a.online && a.status === "active")
  );
  const offlineNames = new Set(
    (clients.data?.clients ?? []).filter((c) => !c.online || c.status !== "active").map((c) => c.name)
  );

  return (
    <div className="space-y-6">
      <PageHeader
        title="Overview"
        desc={`${online.length} of ${d?.clients_total ?? 0} clients online · live via event stream`}
        actions={
          <span className="flex items-center gap-3">
            <WindowPicker value={win} onChange={setWin} />
            <LiveBadge live={online.length > 0} />
          </span>
        }
      />
      <div className="stagger grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-6">
        <Stat label="Clients online" value={<span className="flex items-center gap-2"><Cpu size={18} className="text-ok" />{online.length}<span className="text-sm text-muted">/ {d?.clients_total ?? 0}</span></span>} />
        <Stat label="Virtual models" value={<span className="flex items-center gap-2"><Boxes size={18} className="text-accent" />{d?.virtual_models ?? 0}</span>} />
        <Stat label="Queued" value={
          <span className="flex items-center gap-2"><ListOrdered size={18} className="text-warn" />{d?.queued ?? 0}
            {(d?.queued_by_class ?? []).some((n) => n > 0) && (
              <span className="flex gap-1">
                {d!.queued_by_class!.map((n, i) => n > 0 && (
                  <Badge key={i} tone={prioTone(i)}>{`${PRIORITY_LABELS[i] ?? `Class ${i}`} ${n}`}</Badge>
                ))}
              </span>
            )}
          </span>
        } />
        <Stat label="In flight" value={<span className="flex items-center gap-2"><Activity size={18} className="text-accent" />{d?.inflight ?? 0}</span>} />
        <Stat label="Requests (24h)" value={<span className="flex items-center gap-2"><Zap size={18} className="text-ok" />{fmtNum(d?.requests_24h ?? 0)}</span>} />
      </div>
      <div className="grid gap-4 md:grid-cols-2">
        <div className="card p-4">
          <div className="mb-3 text-xs font-medium uppercase tracking-wide text-muted">API endpoints</div>
          <div className="space-y-1.5">
            {API_ENDPOINTS.map(e => (
              <div key={e.p} className="flex items-center gap-2.5 text-xs">
                <span className={`w-10 rounded px-1.5 py-0.5 text-center font-mono text-[10px] ${e.m === "GET" ? "bg-ok/10 text-ok" : "bg-accent/10 text-accent"}`}>{e.m}</span>
                <code className="font-mono text-text">{e.p}</code>
                <span className="ml-auto text-[10px] text-muted">{e.d}</span>
              </div>
            ))}
          </div>
        </div>
        <div className="card p-4">
          <div className="mb-3 text-xs font-medium uppercase tracking-wide text-muted">Virtual models</div>
          {(vms.data?.models ?? []).length === 0 && <div className="text-xs text-muted">No virtual models configured.</div>}
          <div className="space-y-1.5">
            {(vms.data?.models ?? []).map(m => (
              <div key={m.id} className="flex items-center gap-2.5 text-xs">
                <span className={`h-1.5 w-1.5 rounded-full ${m.enabled ? "bg-ok" : "bg-muted"}`} />
                <code className="font-mono text-text">{m.api_name}</code>
                <span className="rounded bg-surface px-1.5 py-0.5 text-[10px] text-muted">{m.api_type}</span>
              </div>
            ))}
          </div>
        </div>
      </div>
      {fleet.length > 0 && (
        <div className="card p-4">
          <div className="mb-3 text-xs font-medium uppercase tracking-wide text-muted">Fleet load — live per client</div>
          <div className="grid gap-x-8 gap-y-2 sm:grid-cols-2">
            {fleet.map(c => {
              const s = c.stats;
              const dim = !c.online || c.status !== "active";
              const gpuPct = s?.gpu?.mem_total_bytes ? (s.gpu.mem_used_bytes ?? 0) / s.gpu.mem_total_bytes * 100 : s?.gpu?.util_pct ?? null;
              const row = (label: string, pct: number | null | undefined) => (
                <div key={label} className="flex items-center gap-2 text-[11px]">
                  <span className="w-8 text-muted">{label}</span>
                  <div className="h-1.5 flex-1 rounded bg-white/10">
                    {pct != null && <div className={`h-1.5 rounded ${dim ? "bg-slate-500" : pct >= 85 ? "bg-rose-400" : pct >= 65 ? "bg-amber-300" : "bg-emerald-400"}`} style={{ width: `${Math.min(100, pct)}%` }} />}
                  </div>
                  <span className="w-8 text-right text-muted">{pct != null ? `${fmtPct(pct)}%` : "—"}</span>
                </div>
              );
              return (
                <div key={c.id} className={`rounded-md bg-surface/60 px-3 py-2 ${dim ? "opacity-45" : ""}`}>
                  <div className="mb-1 flex items-center gap-2 text-xs">
                    <span className={`h-1.5 w-1.5 rounded-full ${c.status === "suspended" ? "bg-amber-400" : c.online ? "bg-ok" : "bg-slate-500"}`} />
                    <span className="font-medium">{c.name}</span>
                    {dim && (
                      <span className={`rounded px-1.5 py-0.5 text-[10px] ${c.status !== "active" ? "bg-amber-400/15 text-amber-300" : "bg-slate-500/15 text-slate-400"}`}>
                        {c.status !== "active" ? c.status : "offline"}
                      </span>
                    )}
                    {s?.gpu && <span className="ml-auto truncate text-[10px] text-muted" title={s.gpu.name}>{s.gpu.name}</span>}
                  </div>
                  {dim && !s ? (
                    <div className="text-[10px] text-muted">no live stats</div>
                  ) : (
                    <>
                      {row("CPU", s?.cpu_pct)}
                      {row("RAM", s?.ram_pct)}
                      {s?.gpu && row(s.gpu.util_pct != null ? "GPU" : "VRAM", gpuPct)}
                    </>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
      {chart.length > 1 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">Requests</div>
          <ResponsiveContainer width="100%" height={160}>
            <ComposedChart data={chart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
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
      <div className="grid gap-4 md:grid-cols-3">
        {ttftChart.length > 1 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">avg TTFT</div>
            <ResponsiveContainer width="100%" height={130}>
              <AreaChart data={ttftChart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={(v) => fmtMs(Number(v))} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtMs(Number(v))} />
                <Area type="monotone" dataKey="ttft" stroke="#5b7eae" fill="#5b7eae" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
              </AreaChart>
            </ResponsiveContainer>
          </div>
        )}
        {latChart.length > 1 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">latency percentiles</div>
            <ResponsiveContainer width="100%" height={130}>
              <AreaChart data={latChart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={(v) => fmtMs(Number(v))} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtMs(Number(v))} />
                <Legend wrapperStyle={{ fontSize: 11 }} />
                <Area type="monotone" dataKey="p50" stroke="#34d399" fill="#34d399" fillOpacity={0.10} strokeWidth={2} isAnimationActive={false} />
                <Area type="monotone" dataKey="p95" stroke="#fbbf24" fill="#fbbf24" fillOpacity={0.10} strokeWidth={2} isAnimationActive={false} />
                <Area type="monotone" dataKey="p99" stroke="#fb7185" fill="#fb7185" fillOpacity={0.10} strokeWidth={2} isAnimationActive={false} />
              </AreaChart>
            </ResponsiveContainer>
          </div>
        )}
        {reqPerClient.length > 0 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests / client</div>
            <ResponsiveContainer width="100%" height={130}>
              <BarChart data={reqPerClient} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} tickFormatter={fmtNum} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
                <Bar dataKey="n" fill="#3d6fae" radius={[4, 4, 0, 0]}>
                  {reqPerClient.map(e => <Cell key={e.name} fill={offlineNames.has(e.name) ? "#33415c" : "#3d6fae"} />)}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>
      {tokSeries.length > 0 && (
        <div className="card p-4">
          <div className="mb-2 flex items-baseline justify-between">
            <div className="text-xs font-medium uppercase tracking-wide text-muted">tokens / min</div>
            <div className="text-[10px] text-muted">e2e tok/s = total tokens ÷ total request time</div>
          </div>
          <ResponsiveContainer width="100%" height={170}>
            <ComposedChart data={tokSeries} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis yAxisId="l" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
              <YAxis yAxisId="r" orientation="right" tick={{ fontSize: 10, fill: "#8fb3d9" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
              <Legend wrapperStyle={{ fontSize: 11 }} />
              <Bar yAxisId="l" dataKey="pt" name="prompt" stackId="tok" fill="#3d6fae" />
              <Bar yAxisId="l" dataKey="ct" name="completion" stackId="tok" fill="#7fa8d9" radius={[4, 4, 0, 0]} />
              <Area yAxisId="r" dataKey="tps" name="tok/s" stroke="#8fb3d9" fill="none" strokeWidth={1.5} dot={false} isAnimationActive={false} />
            </ComposedChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="grid gap-4 md:grid-cols-2">
        {tpsModel.length > 0 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tok/s by model <span className="normal-case text-muted/70">(e2e vs decode)</span></div>
            <ResponsiveContainer width="100%" height={150}>
              <BarChart data={tpsModel} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
                <Legend wrapperStyle={{ fontSize: 11 }} />
                <Bar dataKey="e2e" name="e2e tok/s" fill="#3d6fae" radius={[4, 4, 0, 0]} />
                <Bar dataKey="dec" name="decode tok/s" fill="#4a86c8" radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
        {tpsClient.length > 0 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tok/s by client <span className="normal-case text-muted/70">(e2e vs decode)</span></div>
            <ResponsiveContainer width="100%" height={150}>
              <BarChart data={tpsClient} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} tickFormatter={fmtNum} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
                <Legend wrapperStyle={{ fontSize: 11 }} />
                <Bar dataKey="e2e" name="e2e tok/s" fill="#3d6fae" radius={[4, 4, 0, 0]}>
                  {tpsClient.map(e => <Cell key={e.name} fill={offlineNames.has(e.name) ? "#33415c" : "#3d6fae"} />)}
                </Bar>
                <Bar dataKey="dec" name="decode tok/s" fill="#4a86c8" radius={[4, 4, 0, 0]}>
                  {tpsClient.map(e => <Cell key={e.name} fill={offlineNames.has(e.name) ? "#3a4a68" : "#4a86c8"} />)}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
        {perClient.length > 0 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">tier-2 capacity / client</div>
            <ResponsiveContainer width="100%" height={130}>
              <BarChart data={perClient} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} tickFormatter={fmtNum} />
                <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} formatter={(v) => fmtNum(Number(v))} />
                <Bar dataKey="cap" fill="#3d6fae" radius={[4, 4, 0, 0]}>
                  {perClient.map(e => <Cell key={e.name} fill={offlineNames.has(e.name) ? "#33415c" : "#3d6fae"} />)}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>
      <div className="card">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Connected clients</div>
        <div className="overflow-x-auto"><table className="w-full">
          <thead><tr><th className="th">Name</th><th className="th">Status</th><th className="th">Version</th><th className="th">Platform</th><th className="th">Last seen</th></tr></thead>
          <tbody>
            {(clients.data?.clients ?? []).map((c) => (
              <tr key={c.id} className={!c.online || c.status !== "active" ? "opacity-50" : ""}>
                <td className="td">{c.name}</td>
                <td className="td">{c.status !== "active" ? <span className="rounded bg-amber-400/15 px-1.5 py-0.5 text-[10px] text-amber-300">{c.status}</span> : <LiveBadge live={c.online} />}</td>
                <td className="td text-muted">{c.version || "—"}</td>
                <td className="td text-muted">{c.platform || "—"}</td>
                <td className="td text-muted">{c.last_seen_at ? fmtTs(c.last_seen_at) : "—"}</td>
              </tr>
            ))}
            {(clients.data?.clients ?? []).length === 0 && (
              <tr><td className="td text-center text-muted" colSpan={5}>No clients registered</td></tr>
            )}
          </tbody>
        </table></div>
      </div>
    </div>
  );
}
