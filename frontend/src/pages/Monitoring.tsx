import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import { Card, Spinner } from "../components/ui";
import type { HistoryRow, ApiUser, UsageStat } from "../lib/types";
import { fmtTs, fmtTimeS } from "../lib/format";
import {
  Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";

const TOOLTIP_STYLE = { background: "#10161d", border: "1px solid #1f2a36", borderRadius: 6, color: "#dbe4ee" } as const;

export default function MonitoringPage() {
  const usage = useQuery({ queryKey: ["usage"], queryFn: () => api.get<{ usage_24h: UsageStat[] }>("/api/stats/usage"), refetchInterval: 10000 });
  const hist = useQuery({ queryKey: ["hist"], queryFn: () => api.get<{ requests: HistoryRow[] }>("/api/history?limit=200"), refetchInterval: 5000 });
  const versions = useQuery({ queryKey: ["cfgvers"], queryFn: () => api.get<{ versions: { id: number; entity: string; entity_id: string; change: unknown; actor: string; created_at: string }[] }>("/api/config-versions"), refetchInterval: 15000 });
  const projects = useQuery({ queryKey: ["projects"], queryFn: () => api.get<{ projects: ApiUser[] }>("/api/projects") });
  const pname = (id: string) => projects.data?.projects.find((p) => p.id === id)?.name ?? id.slice(0, 8);

  const data = (usage.data?.usage_24h ?? []).map((u) => ({
    project: pname(u.project_id),
    requests: u.requests,
    tokens: u.prompt_tokens + u.completion_tokens,
  }));

  const rows = hist.data?.requests ?? [];
  const ttfts = rows.filter((r) => r.ttft_ms > 0).map((r) => r.ttft_ms).sort((a, b) => a - b);
  const p50 = ttfts[Math.floor(ttfts.length * 0.5)] ?? 0;
  const p99 = ttfts[Math.floor(ttfts.length * 0.99)] ?? 0;
  const failures = rows.filter((r) => r.status === "failed").length;

  return (
    <div className="space-y-6">
      <h1 className="text-lg font-semibold">Monitoring</h1>
      <div className="grid grid-cols-3 gap-4">
        <Card><div className="text-xs text-muted">TTFT p50</div><div className="mt-1 text-2xl font-semibold">{p50}<span className="text-sm text-muted"> ms</span></div></Card>
        <Card><div className="text-xs text-muted">TTFT p99</div><div className="mt-1 text-2xl font-semibold">{p99}<span className="text-sm text-muted"> ms</span></div></Card>
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
              <YAxis stroke="#8b9bad" fontSize={12} />
              <Tooltip cursor={{ fill: "rgba(61,111,174,0.09)", stroke: "#4a86c8", strokeOpacity: 0.35 }} contentStyle={TOOLTIP_STYLE} />
              <Bar dataKey="requests" fill="#5b8def" radius={[3, 3, 0, 0]} />
              <Bar dataKey="tokens" fill="#3fb96c" radius={[3, 3, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        )}
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
                <td className="td text-muted">{r.queue_wait_ms}ms</td>
                <td className="td text-muted">{r.ttft_ms}ms</td>
                <td className="td text-muted">{r.duration_ms}ms</td>
                <td className="td text-muted">{r.prompt_tokens}+{r.completion_tokens}</td>
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
    </div>
  );
}
