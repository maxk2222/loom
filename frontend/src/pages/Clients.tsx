import { useQuery } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Spinner } from "../components/ui";
import type { ClientRow } from "../lib/types";
import { fmtPct, fmtTs } from "../lib/format";

export default function ClientsPage() {
  const q = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const [enroll, setEnroll] = useState(false);
  const [label, setLabel] = useState("");
  const [maxUses, setMaxUses] = useState(1);
  const [hours, setHours] = useState(24);
  const [issued, setIssued] = useState("");

  const issue = async (e: FormEvent) => {
    e.preventDefault();
    const r = await api.post<{ token: string }>("/api/enroll-tokens", {
      label, max_uses: maxUses, expires_hours: hours,
    });
    setIssued(r.token);
  };

  const setStatus = async (id: string, status: string) => {
    await api.post(`/api/clients/${id}/status`, { status });
    await q.refetch();
  };
  const pushUpdate = async (id: string) => {
    const r = await api.post<{ live?: boolean }>(`/api/clients/${id}/update`).catch(() => null);
    if (r) alert(r.live ? "Update pushed to the live session." : "Update queued — applies on the client's next check-in.");
    q.refetch();
  };

  const Bar = ({ pct, title }: { pct: number | null | undefined; title: string }) =>
    pct == null ? <span className="text-muted text-xs">—</span> : (
      <div className="w-20" title={`${title} ${fmtPct(pct)}%`}>
        <div className="flex justify-between text-[10px] text-muted"><span>{title}</span><span>{fmtPct(pct)}%</span></div>
        <div className="h-1.5 rounded bg-white/10">
          <div className={`h-1.5 rounded ${pct >= 85 ? "bg-rose-400" : pct >= 65 ? "bg-amber-300" : "bg-emerald-400"}`} style={{ width: `${Math.min(100, pct)}%` }} />
        </div>
      </div>
    );

  const StatsCell = ({ c }: { c: ClientRow }) => {
    const s = c.stats;
    if (!c.online || !s || s.cpu_pct == null) return <span className="text-muted text-xs">—</span>;
    const g = s.gpu;
    const gp = g?.mem_total_bytes ? (g.mem_used_bytes ?? 0) / g.mem_total_bytes * 100 : g?.util_pct ?? null;
    return (
      <div className="flex flex-col gap-1">
        <Bar pct={s.cpu_pct} title="CPU" />
        <Bar pct={s.ram_pct} title="RAM" />
        {g && <Bar pct={gp} title={g.util_pct != null ? "GPU" : "VRAM"} />}
        {g && <span className="text-[10px] text-muted" title={g.name}>{g.name.length > 22 ? g.name.slice(0, 22) + "…" : g.name}</span>}
      </div>
    );
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Clients</h1>
        <button className="btn-primary" onClick={() => { setEnroll(true); setIssued(""); }}>
          <Plus size={16} /> Issue enroll token
        </button>
      </div>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-x-auto p-0">
          <table className="w-full">
            <thead><tr><th className="th">Name</th><th className="th">State</th><th className="th">Status</th><th className="th">Version</th><th className="th">Platform</th><th className="th">Load</th><th className="th">Last seen</th><th className="th" /></tr></thead>
            <tbody>
              {(q.data?.clients ?? []).map((c) => (
                <tr key={c.id}>
                  <td className="td font-medium">{c.name}</td>
                  <td className="td">{c.online ? <Badge tone="ok">online</Badge> : <Badge tone="muted">offline</Badge>}</td>
                  <td className="td"><Badge tone={c.status === "active" ? "accent" : "warn"}>{c.status}</Badge></td>
                  <td className="td text-muted">{c.version || "—"}</td>
                  <td className="td text-muted">{c.platform || "—"}</td>
                  <td className="td"><StatsCell c={c} /></td>
                  <td className="td text-muted">{c.last_seen_at ? fmtTs(c.last_seen_at) : "—"}</td>
                  <td className="td text-right">
                    <span className="inline-flex gap-2">
                      {c.online && (
                        <button className="btn-outline py-1 text-xs" onClick={() => pushUpdate(c.id)}
                          title="Push a client update (downloads + verifies the latest release, then restarts)">
                          Update
                        </button>
                      )}
                      {c.status === "active" ? (
                        <button className="btn-outline py-1 text-xs" onClick={() => setStatus(c.id, "suspended")}>Suspend</button>
                      ) : (
                        <button className="btn-outline py-1 text-xs" onClick={() => setStatus(c.id, "active")}>Activate</button>
                      )}
                    </span>
                  </td>
                </tr>
              ))}
              {(q.data?.clients ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={8}>No clients yet — issue an enroll token and run `loom enroll` on a workstation.</td></tr>}
            </tbody>
          </table>
        </Card>
      )}
      <Modal open={enroll} title="Issue enroll token" onClose={() => setEnroll(false)}>
        {issued ? (
          <div className="space-y-3">
            <p className="text-sm text-muted">One-time token — store it now, it is only shown once.</p>
            <code className="block break-all rounded-md bg-panel2 p-3 text-xs">{issued}</code>
            <p className="text-xs text-muted">
              Run on the workstation: <code>loom enroll --server &lt;url&gt; --token &lt;token&gt;</code>
            </p>
            <button className="btn-primary w-full justify-center" onClick={() => setEnroll(false)}>Done</button>
          </div>
        ) : (
          <form onSubmit={issue} className="space-y-4">
            <Field label="Label"><input className="input" value={label} onChange={(e) => setLabel(e.target.value)} placeholder="workstation-a12" /></Field>
            <div className="grid grid-cols-2 gap-3">
              <Field label="Max uses"><input className="input" type="number" min={1} value={maxUses} onChange={(e) => setMaxUses(+e.target.value)} /></Field>
              <Field label="Expires (hours)"><input className="input" type="number" min={1} value={hours} onChange={(e) => setHours(+e.target.value)} /></Field>
            </div>
            <button className="btn-primary w-full justify-center">Issue token</button>
          </form>
        )}
      </Modal>
    </div>
  );
}
