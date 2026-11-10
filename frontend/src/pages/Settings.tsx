import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Toggle } from "../components/ui";
import type { SystemInfo } from "../lib/types";

interface EndpointHintRow {
  url: string;
  pki: boolean;
  scope: string;
}

export default function SettingsPage() {
  const qc = useQueryClient();
  const branding = useQuery({
    queryKey: ["branding"],
    queryFn: () => api.get<{ app_name: string; tagline: string; accent_color: string; logo_path: string }>("/api/branding"),
  });
  const sched = useQuery({
    queryKey: ["set-sched"],
    queryFn: () => api.get<{ value: Record<string, unknown> | null }>("/api/settings/scheduler"),
  });
  const hints = useQuery({
    queryKey: ["set-hints"],
    queryFn: () => api.get<{ value: EndpointHintRow[] | null }>("/api/settings/endpoint_hints"),
  });
  const sys = useQuery({ queryKey: ["sys"], queryFn: () => api.get<SystemInfo>("/api/system"), refetchInterval: 10000 });

  const [b, setB] = useState({ app_name: "", tagline: "", accent_color: "#5b8def" });
  const [s, setS] = useState({ max_queue: 1000, assign_ack_timeout_ms: 5000, lease_ttl_secs: 120, max_attempts: 3, queue_timeout_secs: 120 });
  const [eps, setEps] = useState<EndpointHintRow[]>([]);
  const [saved, setSaved] = useState("");
  const fileRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (branding.data) setB({ app_name: branding.data.app_name, tagline: branding.data.tagline, accent_color: branding.data.accent_color });
  }, [branding.data]);
  useEffect(() => {
    if (sched.data?.value) setS({ ...s, ...(sched.data.value as typeof s) });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sched.data]);
  useEffect(() => {
    if (hints.data?.value) setEps(hints.data.value);
  }, [hints.data]);

  const saveBranding = async (e: FormEvent) => {
    e.preventDefault();
    await api.put("/api/branding", b);
    setSaved("branding");
    setTimeout(() => setSaved(""), 2000);
    qc.invalidateQueries({ queryKey: ["branding"] });
  };

  const saveSched = async (e: FormEvent) => {
    e.preventDefault();
    await api.put("/api/settings/scheduler", { value: s });
    setSaved("scheduler");
    setTimeout(() => setSaved(""), 2000);
  };

  const saveHints = async (e: FormEvent) => {
    e.preventDefault();
    const clean = eps.filter((x) => x.url.trim() !== "");
    await api.put("/api/settings/endpoint_hints", { value: clean });
    setEps(clean);
    setSaved("hints");
    setTimeout(() => setSaved(""), 2000);
    qc.invalidateQueries({ queryKey: ["set-hints"] });
  };

  const uploadLogo = async (file: File) => {
    const fd = new FormData();
    fd.append("logo", file);
    await api.postForm("/api/branding/logo", fd);
    qc.invalidateQueries({ queryKey: ["branding"] });
  };

  const backup = async () => {
    await api.post("/api/system/backup");
    setSaved("backup");
    setTimeout(() => setSaved(""), 2000);
  };

  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-lg font-semibold">Settings</h1>

      <Card className="space-y-4">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-medium">Branding</h2>
          {saved === "branding" && <Badge tone="ok">saved</Badge>}
        </div>
        <form onSubmit={saveBranding} className="space-y-4">
          <Field label="App name"><input className="input" value={b.app_name} onChange={(e) => setB({ ...b, app_name: e.target.value })} /></Field>
          <Field label="Tagline"><input className="input" value={b.tagline} onChange={(e) => setB({ ...b, tagline: e.target.value })} /></Field>
          <Field label="Accent color" hint="#rrggbb">
            <div className="flex items-center gap-3">
              <input type="color" className="h-9 w-14 cursor-pointer rounded border border-border bg-panel2" value={b.accent_color} onChange={(e) => setB({ ...b, accent_color: e.target.value })} />
              <input className="input font-mono" value={b.accent_color} onChange={(e) => setB({ ...b, accent_color: e.target.value })} />
            </div>
          </Field>
          <Field label="Logo" hint="PNG/JPG/WebP, max 2 MB">
            <div className="flex items-center gap-3">
              {branding.data?.logo_path && <img src={branding.data.logo_path} alt="logo" className="h-10 w-10 rounded object-contain" />}
              <input ref={fileRef} type="file" accept="image/png,image/jpeg,image/webp" className="hidden"
                onChange={(e) => e.target.files?.[0] && uploadLogo(e.target.files[0])} />
              <button type="button" className="btn-outline" onClick={() => fileRef.current?.click()}>Upload</button>
            </div>
          </Field>
          <button className="btn-primary">Save branding</button>
        </form>
      </Card>

      <Card className="space-y-4">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-medium">Scheduler</h2>
          {saved === "scheduler" && <Badge tone="ok">saved</Badge>}
        </div>
        <form onSubmit={saveSched} className="grid grid-cols-1 sm:grid-cols-2 gap-4">
          <Field label="Max queue depth"><input className="input" type="number" value={s.max_queue} onChange={(e) => setS({ ...s, max_queue: +e.target.value })} /></Field>
          <Field label="Queue timeout (s)"><input className="input" type="number" value={s.queue_timeout_secs} onChange={(e) => setS({ ...s, queue_timeout_secs: +e.target.value })} /></Field>
          <Field label="Assign-ack timeout (ms)"><input className="input" type="number" value={s.assign_ack_timeout_ms} onChange={(e) => setS({ ...s, assign_ack_timeout_ms: +e.target.value })} /></Field>
          <Field label="Lease TTL (s)"><input className="input" type="number" value={s.lease_ttl_secs} onChange={(e) => setS({ ...s, lease_ttl_secs: +e.target.value })} /></Field>
          <Field label="Max attempts"><input className="input" type="number" min={1} value={s.max_attempts} onChange={(e) => setS({ ...s, max_attempts: +e.target.value })} /></Field>
          <div className="col-span-2"><button className="btn-primary">Save scheduler</button></div>
        </form>
      </Card>

      <Card className="space-y-4">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-medium">Endpoint hints</h2>
          {saved === "hints" && <Badge tone="ok">saved</Badge>}
        </div>
        <p className="text-xs text-muted">
          Pushed to every client in HelloAck — a client enrolled via the public
          endpoint learns LAN/Tailscale endpoints automatically. Order:
          non-public first, configured entries keep precedence, public last.
        </p>
        <form onSubmit={saveHints} className="space-y-3">
          {eps.map((ep, i) => (
            <div key={i} className="grid grid-cols-1 gap-2 sm:grid-cols-[1fr_120px_90px_36px] sm:items-end">
              <Field label={i === 0 ? "URL" : ""}>
                <input
                  className="input font-mono text-xs"
                  placeholder="https://host:50052"
                  value={ep.url}
                  onChange={(e) => setEps(eps.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))}
                />
              </Field>
              <Field label={i === 0 ? "Scope" : ""}>
                <select
                  className="input"
                  value={ep.scope}
                  onChange={(e) => setEps(eps.map((x, j) => (j === i ? { ...x, scope: e.target.value } : x)))}
                >
                  <option value="lan">lan</option>
                  <option value="wan">wan</option>
                  <option value="public">public</option>
                </select>
              </Field>
              <Field label={i === 0 ? "PKI" : ""}>
                <Toggle
                  checked={ep.pki}
                  onChange={(v) => setEps(eps.map((x, j) => (j === i ? { ...x, pki: v } : x)))}
                  label=""
                />
              </Field>
              <button
                type="button"
                className="btn-outline h-9 px-2 text-xs text-red-400"
                onClick={() => setEps(eps.filter((_, j) => j !== i))}
              >
                ×
              </button>
            </div>
          ))}
          <div className="flex gap-2">
            <button
              type="button"
              className="btn-outline"
              onClick={() => setEps([...eps, { url: "", pki: true, scope: "lan" }])}
            >
              Add endpoint
            </button>
            <button className="btn-primary">Save hints</button>
          </div>
        </form>
      </Card>

      <Card className="space-y-3">
        <h2 className="text-sm font-medium">System</h2>
        <div className="flex items-center gap-4 text-sm">
          <span className="text-muted">Server v{sys.data?.version ?? "—"}</span>
          <span className="flex items-center gap-1.5">Postgres {sys.data?.postgres ? <Badge tone="ok">ok</Badge> : <Badge tone="err">down</Badge>}</span>
          <span className="flex items-center gap-1.5">Redis {sys.data?.redis ? <Badge tone="ok">ok</Badge> : <Badge tone="err">down</Badge>}</span>
        </div>
        <div className="flex items-center gap-3 pt-1">
          <button className="btn-outline" onClick={backup}>Backup database</button>
          {saved === "backup" && <Badge tone="ok">done</Badge>}
        </div>
      </Card>

      <ThemeNote />
    </div>
  );
}

function ThemeNote() {
  return (
    <Card>
      <h2 className="mb-1 text-sm font-medium">Theme</h2>
      <p className="text-sm text-muted">LOOM ships a dark-only interface optimized for readability in research environments. Branding adjusts the accent color globally.</p>
      <div className="mt-3 flex items-center gap-2">
        <Toggle checked={true} onChange={() => {}} label="dark mode" />
        <Badge tone="accent">dark-only</Badge>
      </div>
    </Card>
  );
}
