import { useQuery } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../lib/api";
import { useBranding } from "../lib/branding";

function BrandHeader() {
  const b = useBranding();
  return (
    <div className="mb-8 text-center">
      {b.logo_path ? (
        <img src={b.logo_path} alt="" className="mx-auto mb-3 h-12 w-12 rounded object-contain" />
      ) : (
        <div className="mx-auto mb-3 flex h-12 w-12 items-center justify-center rounded-lg bg-accent text-xl font-bold text-white">
          {b.app_name.slice(0, 1)}
        </div>
      )}
      <h1 className="text-xl font-semibold">{b.app_name}</h1>
      <p className="mt-1 text-sm text-muted">{b.tagline}</p>
    </div>
  );
}

export function Login() {
  const nav = useNavigate();
  const [email, setEmail] = useState("");
  const [pw, setPw] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr("");
    try {
      await api.post("/api/auth/login", { email, password: pw });
      nav("/");
    } catch {
      setErr("Invalid credentials");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <BrandHeader />
        <form onSubmit={submit} className="card space-y-4 p-6">
          <div>
            <label className="label">Email</label>
            <input className="input" type="email" autoFocus value={email} onChange={(e) => setEmail(e.target.value)} required />
          </div>
          <div>
            <label className="label">Password</label>
            <input className="input" type="password" value={pw} onChange={(e) => setPw(e.target.value)} required />
          </div>
          {err && <div className="text-sm text-err">{err}</div>}
          <button className="btn-primary w-full justify-center" disabled={busy}>
            {busy ? "Signing in…" : "Sign in"}
          </button>
        </form>
      </div>
    </div>
  );
}

export function Setup() {
  const nav = useNavigate();
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [pw, setPw] = useState("");
  const [pw2, setPw2] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (pw !== pw2) return setErr("Passwords do not match");
    setBusy(true);
    setErr("");
    try {
      await api.post("/api/setup/admin", { email, password: pw, display_name: name });
      nav("/login");
    } catch (e: any) {
      setErr(e?.message ?? "Setup failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <BrandHeader />
        <form onSubmit={submit} className="card space-y-4 p-6">
          <h2 className="text-sm font-medium text-muted">Create the first administrator</h2>
          <div>
            <label className="label">Display name</label>
            <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div>
            <label className="label">Email</label>
            <input className="input" type="email" value={email} onChange={(e) => setEmail(e.target.value)} required />
          </div>
          <div>
            <label className="label">Password (min. 8 chars)</label>
            <input className="input" type="password" minLength={8} value={pw} onChange={(e) => setPw(e.target.value)} required />
          </div>
          <div>
            <label className="label">Confirm password</label>
            <input className="input" type="password" minLength={8} value={pw2} onChange={(e) => setPw2(e.target.value)} required />
          </div>
          {err && <div className="text-sm text-err">{err}</div>}
          <button className="btn-primary w-full justify-center" disabled={busy}>
            {busy ? "Creating…" : "Create admin"}
          </button>
        </form>
      </div>
    </div>
  );
}

export function SetupGate({ children }: { children: React.ReactNode }) {
  const { data, isLoading } = useQuery({
    queryKey: ["setup"],
    queryFn: () => api.get<{ needs_setup: boolean }>("/api/setup/status"),
    retry: false,
  });
  if (isLoading) return <div className="flex min-h-screen items-center justify-center text-muted">Loading…</div>;
  if (data?.needs_setup) return <Setup />;
  return <>{children}</>;
}
