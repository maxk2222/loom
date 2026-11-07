import { useQuery } from "@tanstack/react-query";
import { Activity, Boxes, Cpu, ListOrdered, Zap } from "lucide-react";
import { AreaChart, Area, BarChart, Bar, ResponsiveContainer, XAxis, YAxis, Tooltip, CartesianGrid } from "recharts";
import { useEffect, useState } from "react";
import { api, openEventStream } from "../lib/api";
import { LiveBadge, PageHeader, Stat } from "../components/ui";
import type { ClientRow, Overview } from "../lib/types";

export default function OverviewPage() {
  const ov = useQuery({ queryKey: ["overview"], queryFn: () => api.get<Overview>("/api/overview"), refetchInterval: 5000 });
  const clients = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const hist = useQuery({ queryKey: ["ov-hist"], queryFn: () => api.get<{ requests: any[] }>("/api/history?limit=200"), refetchInterval: 10000 });
  const chart = (() => {
    const buckets = new Map<string, { t: string; n: number; ok: number }>();
    for (const h of [...(hist.data?.requests ?? [])].reverse()) {
      const t = new Date(h.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      const b = buckets.get(t) ?? { t, n: 0, ok: 0 };
      b.n++; if (h.status === "done") b.ok++;
      buckets.set(t, b);
    }
    return [...buckets.values()].slice(-30);
  })();
  const ttftChart = (() => {
    const buckets = new Map<string, { t: string; ttft: number; n: number }>();
    for (const h of [...(hist.data?.requests ?? [])].reverse()) {
      if (!h.ttft_ms) continue;
      const t = new Date(h.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      const b = buckets.get(t) ?? { t, ttft: 0, n: 0 };
      b.ttft += h.ttft_ms; b.n++; buckets.set(t, b);
    }
    return [...buckets.values()].map(b => ({ t: b.t, ttft: Math.round(b.ttft / b.n) })).slice(-30);
  })();
  const lat = [...(hist.data?.requests ?? [])].map(h => h.duration_ms ?? 0).filter(Boolean).sort((a, b) => a - b);
  const pct = (p: number) => lat.length ? Math.round(lat[Math.min(lat.length - 1, Math.floor(p * lat.length))]) : 0;
  const depCap = useQuery({ queryKey: ["deps"], queryFn: () => api.get<{ deployments: any[] }>("/api/deployments") });
  const latChart = (() => {
    const m = new Map<string, { t: string; ms: number; n: number }>();
    for (const h of [...(hist.data?.requests ?? [])].reverse()) {
      if (!h.duration_ms) continue;
      const t = new Date(h.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      const b = m.get(t) ?? { t, ms: 0, n: 0 };
      b.ms += h.duration_ms; b.n++; m.set(t, b);
    }
    return [...m.values()].map(b => ({ t: b.t, ms: Math.round(b.ms / b.n) })).slice(-30);
  })();
  const reqPerClient = (() => {
    const m = new Map<string, { name: string; n: number }>();
    for (const h of hist.data?.requests ?? []) {
      if (!h.client_id) continue;
      const n = clients.data?.clients.find(c => c.id === h.client_id)?.name ?? "?";
      const b = m.get(n) ?? { name: n, n: 0 };
      b.n++; m.set(n, b);
    }
    return [...m.values()];
  })();
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

  return (
    <div className="space-y-6">
      <PageHeader
        title="Overview"
        desc={`${online.length} of ${d?.clients_total ?? 0} clients online · live via event stream`}
        actions={<LiveBadge live={online.length > 0} />}
      />
      <div className="stagger grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-6">
        <Stat label="Clients online" value={<span className="flex items-center gap-2"><Cpu size={18} className="text-ok" />{online.length}<span className="text-sm text-muted">/ {d?.clients_total ?? 0}</span></span>} />
        <Stat label="Virtual models" value={<span className="flex items-center gap-2"><Boxes size={18} className="text-accent" />{d?.virtual_models ?? 0}</span>} />
        <Stat label="Queued (5m)" value={<span className="flex items-center gap-2"><ListOrdered size={18} className="text-warn" />{d?.queued_5m ?? 0}</span>} />
        <Stat label="Active (5m)" value={<span className="flex items-center gap-2"><Activity size={18} className="text-accent" />{d?.active_5m ?? 0}</span>} />
        <Stat label="Requests (24h)" value={<span className="flex items-center gap-2"><Zap size={18} className="text-ok" />{d?.requests_24h ?? 0}</span>} />
      </div>
      {chart.length > 1 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">Recent requests (2min)</div>
          <ResponsiveContainer width="100%" height={160}>
            <AreaChart data={chart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} />
              <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
              <Area type="monotone" dataKey="n" stroke="#3d6fae" fill="#3d6fae" fillOpacity={0.16} strokeWidth={2} isAnimationActive={false} />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="grid gap-4 md:grid-cols-3">
        {ttftChart.length > 1 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">avg TTFT (ms)</div>
            <ResponsiveContainer width="100%" height={130}>
              <AreaChart data={ttftChart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
                <Area type="monotone" dataKey="ttft" stroke="#5b7eae" fill="#5b7eae" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
              </AreaChart>
            </ResponsiveContainer>
          </div>
        )}
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">latency (ms)</div>
          <div className="grid grid-cols-3 gap-2 pt-3 text-center">
            <div><div className="text-xl font-semibold text-text">{pct(0.5)}</div><div className="text-[10px] text-muted">p50</div></div>
            <div><div className="text-xl font-semibold text-text">{pct(0.95)}</div><div className="text-[10px] text-muted">p95</div></div>
            <div><div className="text-xl font-semibold text-text">{pct(0.99)}</div><div className="text-[10px] text-muted">p99</div></div>
          </div>
        </div>
        {latChart.length > 1 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">avg latency (ms)</div>
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
        )}
        {reqPerClient.length > 0 && (
          <div className="card p-4">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">requests / client</div>
            <ResponsiveContainer width="100%" height={130}>
              <BarChart data={reqPerClient} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
                <CartesianGrid stroke="#1c2740" vertical={false} />
                <XAxis dataKey="name" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} />
                <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
                <Bar dataKey="n" fill="#3d6fae" radius={[4, 4, 0, 0]} />
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
                <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} />
                <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
                <Bar dataKey="cap" fill="#3d6fae" radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>
      <div className="card">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Connected clients</div>
        <table className="w-full">
          <thead><tr><th className="th">Name</th><th className="th">Status</th><th className="th">Version</th><th className="th">Platform</th><th className="th">Last seen</th></tr></thead>
          <tbody>
            {(clients.data?.clients ?? []).map((c) => (
              <tr key={c.id}>
                <td className="td">{c.name}</td>
                <td className="td"><LiveBadge live={c.online} /></td>
                <td className="td text-muted">{c.version || "—"}</td>
                <td className="td text-muted">{c.platform || "—"}</td>
                <td className="td text-muted">{c.last_seen_at ? new Date(c.last_seen_at).toLocaleString() : "—"}</td>
              </tr>
            ))}
            {(clients.data?.clients ?? []).length === 0 && (
              <tr><td className="td text-center text-muted" colSpan={5}>No clients registered</td></tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
