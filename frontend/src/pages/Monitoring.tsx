import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api";
import { Card, ConfirmModal, Spinner } from "../components/ui";
import { TokenHeatmap } from "../components/Heatmap";
import type { HistoryRow, ApiUser, UsageStat } from "../lib/types";
import { fmtTs, fmtTimeS, fmtMs, fmtNum } from "../lib/format";
import { WindowPicker } from "./Overview";
import {
  Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";

const TOOLTIP_STYLE = { background: "#10161d", border: "1px solid #1f2a36", borderRadius: 6, color: "#dbe4ee" } as const;

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

const DIMS = [
  { v: "models", l: "Model" },
  { v: "clients", l: "Client" },
  { v: "projects", l: "API user" },
  { v: "keys", l: "API key" },
] as const;

export default function MonitoringPage() {
  const [win, setWin] = useState("24h");
  const [dim, setDim] = useState<typeof DIMS[number]["v"]>("models");
  const qc = useQueryClient();
  const [purgeOpen, setPurgeOpen] = useState(false);
  const [purgeResult, setPurgeResult] = useState<string | null>(null);
  const [purgeBusy, setPurgeBusy] = useState(false);
  const purge = async () => {
    setPurgeBusy(true);
    try {
      const r = await api.post<{ deleted_history_rows: Record<string, number>; deleted_redis_keys: number }>("/api/stats/purge-orphans", {});
      const n = Object.values(r.deleted_history_rows).reduce((a, b) => a + b, 0);
      setPurgeOpen(false);
      setPurgeResult(`Purged ${n} history rows and ${r.deleted_redis_keys} runtime keys.`);
      qc.invalidateQueries();
    } finally {
      setPurgeBusy(false);
    }
  };
  const usage = useQuery({ queryKey: ["usage"], queryFn: () => api.get<{ usage_24h: UsageStat[] }>("/api/stats/usage"), refetchInterval: 10000 });
  const hist = useQuery({ queryKey: ["hist"], queryFn: () => api.get<{ requests: HistoryRow[] }>("/api/history?limit=200"), refetchInterval: 5000 });
  const versions = useQuery({ queryKey: ["cfgvers"], queryFn: () => api.get<{ versions: { id: number; entity: string; entity_id: string; change: unknown; actor: string; created_at: string }[] }>("/api/config-versions"), refetchInterval: 15000 });
  const projects = useQuery({ queryKey: ["projects"], queryFn: () => api.get<{ projects: ApiUser[] }>("/api/projects") });
  const brk = useQuery({
    queryKey: ["mon-brk", win],
    queryFn: () => api.get<{ models: BreakdownRow[]; clients: BreakdownRow[]; keys: BreakdownRow[]; projects: BreakdownRow[] }>(`/api/stats/breakdown?window=${win}`),
    refetchInterval: 15000,
  });
  const heatmap = useQuery({
    queryKey: ["tok-heatmap", win],
    queryFn: () => api.get<{ bucket_secs: number; token_buckets: string[]; cells: { t: string; b: number; n: number }[] }>(`/api/stats/token-heatmap?window=${win}`),
    refetchInterval: 15000,
  });
  const pname = (id: string) => projects.data?.projects.find((p) => p.id === id)?.name ?? id.slice(0, 8);

  const data = (usage.data?.usage_24h ?? []).map((u) => ({
    project: pname(u.project_id),
    requests: u.requests,
    tokens: u.prompt_tokens + u.completion_tokens,
  }));

  // Heatmap: server returns {t,b,n} cells — expand to a dense time×bucket
  // grid from first data point to now (empty columns stay dark).
  const heat = (() => {
    const d = heatmap.data;
    if (!d || d.cells.length === 0) return null;
    const bw = d.bucket_secs * 1000;
    const t0 = Math.min(...d.cells.map((c) => new Date(c.t).getTime()));
    const cols = Math.max(1, Math.ceil((Date.now() - t0) / bw));
    const grid = Array.from({ length: d.token_buckets.length }, () => new Array<number>(cols).fill(0));
    let max = 0;
    for (const c of d.cells) {
      const i = Math.min(cols - 1, Math.floor((new Date(c.t).getTime() - t0) / bw));
      grid[c.b][i] += c.n;
      if (grid[c.b][i] > max) max = grid[c.b][i];
    }
    return { grid, max, t0, bw };
  })();

  const rows = hist.data?.requests ?? [];
  const ttfts = rows.filter((r) => r.ttft_ms > 0).map((r) => r.ttft_ms).sort((a, b) => a - b);
  const p50 = ttfts[Math.floor(ttfts.length * 0.5)] ?? 0;
  const p99 = ttfts[Math.floor(ttfts.length * 0.99)] ?? 0;
  const failures = rows.filter((r) => r.status === "failed").length;
  const dimRows = brk.data?.[dim] ?? [];

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Monitoring</h1>
        <div className="flex items-center gap-3">
          <button className="btn-ghost px-2 py-1 text-xs text-muted" onClick={() => setPurgeOpen(true)}>Purge orphaned stats</button>
          <WindowPicker value={win} onChange={setWin} />
        </div>
      </div>
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        <Card><div className="text-xs text-muted">TTFT p50</div><div className="mt-1 text-2xl font-semibold">{fmtMs(p50)}</div></Card>
        <Card><div className="text-xs text-muted">TTFT p99</div><div className="mt-1 text-2xl font-semibold">{fmtMs(p99)}</div></Card>
        <Card><div className="text-xs text-muted">Failures (recent)</div><div className="mt-1 text-2xl font-semibold text-err">{failures}</div></Card>
      </div>
      <Card>
        <div className="mb-3 text-sm font-medium">Requests & tokens per API user (24h)</div>
        {usage.isLoading ? <Spinner /> : data.length === 0 ? (
          <div className="py-10 text-center text-sm text-muted">No usage yet.</div>
        ) : (
          <ResponsiveContainer width="100%" height={260}>
            <BarChart data={data}>
              <CartesianGrid stroke="#1f2a36" strokeDasharray="3 3" />
              <XAxis dataKey="project" stroke="#8b9bad" fontSize={12} />
              <YAxis stroke="#8b9bad" fontSize={12} tickFormatter={fmtNum} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={TOOLTIP_STYLE} formatter={(v) => fmtNum(Number(v))} />
              <Bar dataKey="requests" fill="#5b8def" radius={[3, 3, 0, 0]} />
              <Bar dataKey="tokens" fill="#3fb96c" radius={[3, 3, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        )}
      </Card>
      {heat && (
        <Card>
          <div className="mb-3 text-sm font-medium">Tokens per request <span className="text-xs text-muted">({win}) — cell intensity = requests in bucket</span></div>
          <TokenHeatmap
            grid={heat.grid}
            labels={heatmap.data?.token_buckets ?? []}
            max={heat.max}
            colLabel={(i) => (i % Math.max(1, Math.floor(heat.grid[0].length / 8)) === 0 ? fmtTimeS(heat.t0 + i * heat.bw) : "")}
            title={(r, c, n) => `${n} requests · ${heatmap.data?.token_buckets[r] ?? r} tokens · ${fmtTimeS(heat.t0 + c * heat.bw)}`}
          />
        </Card>
      )}
      <Card className="overflow-x-auto p-0">
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <div className="text-sm font-medium">Token totals <span className="text-xs text-muted">({win})</span></div>
          <div className="flex gap-0.5 rounded-md bg-panel2 p-0.5">
            {DIMS.map(d => (
              <button
                key={d.v}
                onClick={() => setDim(d.v)}
                className={`rounded px-2 py-0.5 text-[11px] transition-colors ${dim === d.v ? "bg-accent/20 text-accent" : "text-muted hover:text-text"}`}
              >{d.l}</button>
            ))}
          </div>
        </div>
        <table className="w-full">
          <thead><tr>
            <th className="th">{DIMS.find(d => d.v === dim)?.l}</th>
            {dim === "keys" && <th className="th">API user</th>}
            <th className="th text-right">Requests</th>
            <th className="th text-right">Prompt</th>
            <th className="th text-right">Completion</th>
            <th className="th text-right">Total</th>
            <th className="th text-right">tok/s</th>
          </tr></thead>
          <tbody>
            {dimRows.map((r) => (
              <tr key={r.name + (r.extra ?? "")}>
                <td className="td">{r.name}</td>
                {dim === "keys" && <td className="td text-muted">{r.extra || "—"}</td>}
                <td className="td text-right text-muted">{fmtNum(r.requests)}</td>
                <td className="td text-right text-muted">{fmtNum(r.prompt_tokens)}</td>
                <td className="td text-right text-muted">{fmtNum(r.completion_tokens)}</td>
                <td className="td text-right font-medium">{fmtNum(r.tokens)}</td>
                <td className="td text-right text-muted">{r.duration_ms > 0 ? fmtNum(+(r.tokens / (r.duration_ms / 1000)).toFixed(1)) : "—"}</td>
              </tr>
            ))}
            {dimRows.length === 0 && <tr><td className="td text-center text-muted" colSpan={dim === "keys" ? 7 : 6}>No usage in this window.</td></tr>}
          </tbody>
        </table>
      </Card>
      <Card className="overflow-x-auto p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Request history</div>
        <table className="w-full">
          <thead><tr><th className="th">Time</th><th className="th">Status</th><th className="th">Queue</th><th className="th">TTFT</th><th className="th">Duration</th><th className="th">Tokens</th><th className="th">Error</th></tr></thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td className="td text-muted">{fmtTimeS(r.created_at)}</td>
                <td className="td"><span className={r.status === "completed" ? "text-ok" : r.status === "failed" ? "text-err" : "text-warn"}>{r.status}</span></td>
                <td className="td text-muted">{fmtMs(r.queue_wait_ms)}</td>
                <td className="td text-muted">{fmtMs(r.ttft_ms)}</td>
                <td className="td text-muted">{fmtMs(r.duration_ms)}</td>
                <td className="td text-muted">{fmtNum(r.prompt_tokens)}+{fmtNum(r.completion_tokens)}</td>
                <td className="td text-muted">{r.error_code || "—"}</td>
              </tr>
            ))}
            {rows.length === 0 && <tr><td className="td text-center text-muted" colSpan={7}>No history.</td></tr>}
          </tbody>
        </table>
      </Card>
      <Card className="overflow-x-auto p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Config history</div>
        <table className="w-full">
          <thead><tr><th className="th">Time</th><th className="th">Entity</th><th className="th">Entity id</th><th className="th">Actor</th></tr></thead>
          <tbody>
            {(versions.data?.versions ?? []).map((v) => (
              <tr key={v.id}>
                <td className="td text-muted">{fmtTs(v.created_at)}</td>
                <td className="td">{v.entity}</td>
                <td className="td font-mono text-xs text-muted">{v.entity_id || "—"}</td>
                <td className="td text-muted">{v.actor}</td>
              </tr>
            ))}
            {(versions.data?.versions ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={4}>No config changes recorded.</td></tr>}
          </tbody>
        </table>
      </Card>
      <ConfirmModal
        open={purgeOpen}
        title="Purge orphaned stats"
        danger
        busy={purgeBusy}
        confirmLabel="Purge"
        body="Delete stats/history rows for models, clients and deployments that no longer exist? This cannot be undone."
        onConfirm={purge}
        onClose={() => setPurgeOpen(false)}
      />
      <ConfirmModal
        open={purgeResult !== null}
        alert
        title="Purge complete"
        confirmLabel="OK"
        body={purgeResult ?? ""}
        onConfirm={() => setPurgeResult(null)}
        onClose={() => setPurgeResult(null)}
      />
    </div>
  );
}
