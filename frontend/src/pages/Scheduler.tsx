import { useQuery } from "@tanstack/react-query";
import { XCircle } from "lucide-react";
import { api } from "../lib/api";
import { Card, Empty, Spinner } from "../components/ui";
import type { HistoryRow, QueueClass } from "../lib/types";

export default function SchedulerPage() {
  const state = useQuery({
    queryKey: ["sched"],
    queryFn: () => api.get<{ queues: QueueClass[] }>("/api/scheduler/state"),
    refetchInterval: 2000,
  });
  const hist = useQuery({
    queryKey: ["hist"],
    queryFn: () => api.get<{ requests: HistoryRow[] }>("/api/history?limit=50"),
    refetchInterval: 3000,
  });

  const cancel = async (id: string) => {
    await api.post(`/api/requests/${id}/cancel`);
    hist.refetch();
  };

  return (
    <div className="space-y-6">
      <h1 className="text-lg font-semibold">Scheduler</h1>
      <div className="grid grid-cols-4 gap-4">
        {(state.data?.queues ?? []).map((q) => (
          <Card key={q.class}>
            <div className="text-xs text-muted">Priority class {q.class}</div>
            <div className="mt-1 text-2xl font-semibold">{q.depth}</div>
            <div className="text-xs text-muted">queued</div>
          </Card>
        ))}
        {(state.data?.queues ?? []).length === 0 && <Card className="col-span-4"><Empty msg="No queue data" /></Card>}
      </div>
      <Card className="overflow-x-auto p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Recent requests</div>
        {hist.isLoading ? <Spinner /> : (
          <table className="w-full">
            <thead><tr>
              <th className="th">Request</th><th className="th">Status</th><th className="th">Queue</th>
              <th className="th">TTFT</th><th className="th">Duration</th><th className="th">Tokens</th><th className="th" />
            </tr></thead>
            <tbody>
              {(hist.data?.requests ?? []).map((r) => (
                <tr key={r.id}>
                  <td className="td font-mono text-xs text-muted">{r.id.slice(0, 8)}…</td>
                  <td className="td">
                    <span className={r.status === "completed" ? "text-ok" : r.status === "failed" ? "text-err" : "text-warn"}>{r.status}</span>
                    {r.error_code && <div className="text-xs text-muted">{r.error_code}</div>}
                  </td>
                  <td className="td text-muted">{r.queue_wait_ms}ms</td>
                  <td className="td text-muted">{r.ttft_ms}ms</td>
                  <td className="td text-muted">{r.duration_ms}ms</td>
                  <td className="td text-muted">{r.prompt_tokens}+{r.completion_tokens}</td>
                  <td className="td text-right">
                    {(r.status === "queued" || r.status === "streaming") && (
                      <button className="btn-ghost p-1 text-err" title="cancel" onClick={() => cancel(r.id)}><XCircle size={15} /></button>
                    )}
                  </td>
                </tr>
              ))}
              {(hist.data?.requests ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={7}>No requests yet.</td></tr>}
            </tbody>
          </table>
        )}
      </Card>
    </div>
  );
}
