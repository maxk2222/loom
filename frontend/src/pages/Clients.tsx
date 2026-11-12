import { useQuery } from "@tanstack/react-query";
import { Check, Copy, ExternalLink, Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api, copyText } from "../lib/api";
import { Badge, Card, ConfirmModal, Field, Modal, Spinner } from "../components/ui";
import type { ClientRow } from "../lib/types";
import { fmtPct, fmtTs } from "../lib/format";

function CopyCmd({ cmd, hint }: { cmd: string; hint?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div>
      <div className="relative">
        <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-panel2 p-3 pr-20 font-mono text-xs">{cmd}</pre>
        <button
          className="btn-ghost absolute right-2 top-2 inline-flex items-center gap-1 px-2 py-1 text-[11px]"
          onClick={async () => {
            await copyText(cmd);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          }}
        >
          {copied ? <Check size={12} className="text-ok" /> : <Copy size={12} />} {copied ? "copied" : "copy"}
        </button>
      </div>
      {hint && <p className="mt-1.5 text-xs text-muted">{hint}</p>}
    </div>
  );
}

export default function ClientsPage() {
  const q = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const [enroll, setEnroll] = useState(false);
  const [label, setLabel] = useState("");
  const [maxUses, setMaxUses] = useState(1);
  const [hours, setHours] = useState(24);
  const [issued, setIssued] = useState("");
  const [method, setMethod] = useState<"native" | "docker">("native");
  const [tokCopied, setTokCopied] = useState(false);

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
  const [pendingRemove, setPendingRemove] = useState<{ id: string; name: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const remove = async () => {
    if (!pendingRemove) return;
    await api.del(`/api/clients/${pendingRemove.id}`);
    setPendingRemove(null);
    await q.refetch();
  };
  const pushUpdate = async (id: string) => {
    const r = await api.post<{ live?: boolean }>(`/api/clients/${id}/update`).catch(() => null);
    if (r) setNotice(r.live ? "Update pushed to the live session." : "Update queued — applies on the client's next check-in.");
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
        <button className="btn-primary" onClick={() => { setEnroll(true); setIssued(""); setMethod("native"); }}>
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
                  <td className="td font-medium">
                    <span className="inline-flex items-center gap-1.5">
                      {c.name}
                      {c.ui_url && /^https?:\/\//.test(c.ui_url) && (
                        <a
                          href={c.ui_url}
                          target="_blank"
                          rel="noreferrer"
                          className="text-accent hover:underline inline-flex items-center gap-0.5 text-xs font-normal"
                          title={`Local UI: ${c.ui_url}`}
                        >
                          UI <ExternalLink size={11} />
                        </a>
                      )}
                    </span>
                  </td>
                  <td className="td">
                    <span className="inline-flex items-center gap-1.5">
                      {c.online ? <Badge tone="ok">online</Badge> : <Badge tone="muted">offline</Badge>}
                      {c.online && c.via && (
                        <Badge tone={c.via === "plain" ? "warn" : "muted"}>{`via ${c.via}`}</Badge>
                      )}
                    </span>
                  </td>
                  <td className="td"><Badge tone={c.status === "active" ? "accent" : "warn"}>{c.status}</Badge></td>
                  <td className="td text-muted">{c.version || "—"}</td>
                  <td className="td text-muted">{c.platform || "—"}</td>
                  <td className="td"><StatsCell c={c} /></td>
                  <td className="td text-muted">{c.last_seen_at ? fmtTs(c.last_seen_at) : "—"}</td>
                  <td className="td text-right">
                    <span className="inline-flex gap-2">
                      {c.online && (
                        c.platform?.endsWith("-container") ? (
                          <button className="btn-outline py-1 text-xs opacity-50 cursor-not-allowed" disabled
                            title="Container install without volume-backed binary — update by pulling a newer image">
                            Update
                          </button>
                        ) : (
                          <button className="btn-outline py-1 text-xs" onClick={() => pushUpdate(c.id)}
                            title="Push a client update (downloads + verifies the latest release, then restarts)">
                            Update
                          </button>
                        )
                      )}
                      {c.status === "active" ? (
                        <button className="btn-outline py-1 text-xs" onClick={() => setStatus(c.id, "suspended")}>Suspend</button>
                      ) : (
                        <button className="btn-outline py-1 text-xs" onClick={() => setStatus(c.id, "active")}>Activate</button>
                      )}
                      <button className="btn-outline py-1 text-xs text-red-400" onClick={() => setPendingRemove({ id: c.id, name: c.name })}>Remove</button>
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
          <div className="space-y-4">
            <p className="text-sm text-muted">One-time token — store it now, it is only shown once.</p>
            <div className="relative">
              <code className="block break-all rounded-md bg-panel2 p-3 pr-20 font-mono text-xs">{issued}</code>
              <button
                className="btn-ghost absolute right-2 top-2 inline-flex items-center gap-1 px-2 py-1 text-[11px]"
                onClick={async () => {
                  await copyText(issued);
                  setTokCopied(true);
                  setTimeout(() => setTokCopied(false), 1500);
                }}
              >
                {tokCopied ? <Check size={12} className="text-ok" /> : <Copy size={12} />} {tokCopied ? "copied" : "copy"}
              </button>
            </div>
            <div>
              <div className="label mb-1.5">Install method</div>
              <div className="grid grid-cols-2 gap-1 rounded-lg bg-panel2 p-1">
                {(["native", "docker"] as const).map((m) => (
                  <button
                    key={m}
                    className={`rounded-md px-3 py-1.5 text-xs font-medium transition ${method === m ? "bg-card text-text shadow" : "text-muted hover:text-text"}`}
                    onClick={() => setMethod(m)}
                  >
                    {m === "native" ? "Native (systemd/launchd)" : "Docker"}
                  </button>
                ))}
              </div>
            </div>
            {method === "native" ? (
              <>
                <CopyCmd
                  cmd={`curl -fsSL ${location.origin}/install-client.sh | sudo bash -s -- --token ${issued}`}
                  hint="Installs the client daemon — systemd unit on Linux, launchd tray app on macOS. Self-updates via signed releases."
                />
                <p className="text-xs text-muted">
                  Already installed? Enroll manually: <code className="font-mono">loom enroll --server &lt;url&gt; --token &lt;token&gt;</code>
                </p>
              </>
            ) : (
              <>
                <CopyCmd
                  cmd={`curl -fsSL ${location.origin}/install-client.sh | bash -s -- --docker --token ${issued}`}
                  hint="Builds a local image (release binary + entrypoint) and starts it via Docker Compose. Requires Docker + compose plugin on the target; state persists in the named volume loom-client-data."
                />
                <p className="text-xs text-muted">
                  Writes the project to <code className="font-mono">./loom-client-docker/</code> — backend bootstrap vars (LOOM_BACKEND_URL, LOOM_MODELS, …) live in its <code className="font-mono">.env</code> and apply on the container's first start.
                </p>
              </>
            )}
            <button className="btn-primary w-full justify-center" onClick={() => setEnroll(false)}>Done</button>
          </div>
        ) : (
          <form onSubmit={issue} className="space-y-4">
            <Field label="Label"><input className="input" value={label} onChange={(e) => setLabel(e.target.value)} placeholder="workstation-a12" /></Field>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <Field label="Max uses"><input className="input" type="number" min={1} value={maxUses} onChange={(e) => setMaxUses(+e.target.value)} /></Field>
              <Field label="Expires (hours)"><input className="input" type="number" min={1} value={hours} onChange={(e) => setHours(+e.target.value)} /></Field>
            </div>
            <button className="btn-primary w-full justify-center">Issue token</button>
          </form>
        )}
      </Modal>
      <ConfirmModal
        open={pendingRemove !== null}
        title="Remove client"
        danger
        confirmLabel="Remove"
        body={pendingRemove ? `Remove client "${pendingRemove.name}" permanently? Its deployments leave all virtual models and the device credential stops working.` : ""}
        onConfirm={remove}
        onClose={() => setPendingRemove(null)}
      />
      <ConfirmModal
        open={notice !== null}
        alert
        title="Client update"
        confirmLabel="OK"
        body={notice ?? ""}
        onConfirm={() => setNotice(null)}
        onClose={() => setNotice(null)}
      />
    </div>
  );
}
