import { FormEvent, useEffect, useState, type ReactNode } from "react";
import { LOCAL_KEY_STORAGE, NEED_KEY_EVENT, localKey } from "../lib/api";

type Mode = "open" | "setup" | "login";

/**
 * Local auth gate for the client UI. Modes reported by /local/api/auth/status:
 *  - "open": nothing configured → straight through
 *  - "setup": first run → small wizard creates a local user (password, argon2)
 *  - "login": user exists → sign in; an API key works as an alternative
 * Sessions are HttpOnly cookies; any 401 re-opens the gate.
 */
export default function LocalKeyGate({ children }: { children: ReactNode }) {
  const [locked, setLocked] = useState<boolean | null>(null);
  const [mode, setMode] = useState<Mode>("open");
  const [existingUser, setExistingUser] = useState("");
  const [user, setUser] = useState("");
  const [pw, setPw] = useState("");
  const [pw2, setPw2] = useState("");
  const [useKey, setUseKey] = useState(false);
  const [key, setKey] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const probe = async (): Promise<boolean> => {
    try {
      const headers: Record<string, string> = localKey()
        ? { authorization: `Bearer ${localKey()}` }
        : {};
      const res = await fetch("/local/api/auth/status", { headers });
      if (res.ok) {
        const d = await res.json();
        const m: Mode = d.mode ?? "open";
        setMode(m);
        setExistingUser(d.user ?? "");
        if (m === "open") {
          // confirm the API itself is reachable without auth
          const st = await fetch("/local/api/status", { headers });
          setLocked(st.status === 401);
          return st.status !== 401;
        }
        // protected: does our session/key already work?
        const st = await fetch("/local/api/status", { headers });
        setLocked(st.status === 401);
        return st.status !== 401;
      }
      setLocked(false);
      return true;
    } catch {
      setLocked(false);
      return true;
    }
  };

  useEffect(() => {
    probe();
    const onNeed = () => setLocked(true);
    window.addEventListener(NEED_KEY_EVENT, onNeed);
    return () => window.removeEventListener(NEED_KEY_EVENT, onNeed);
  }, []);

  const postAuth = async (path: string, body: unknown): Promise<boolean> => {
    setBusy(true);
    setErr("");
    try {
      const res = await fetch(path, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (res.ok) {
        setLocked(false);
        return true;
      }
      const d = await res.json().catch(() => ({}));
      setErr(d.error ?? `error ${res.status}`);
      return false;
    } catch {
      setErr("local API unreachable");
      return false;
    } finally {
      setBusy(false);
    }
  };

  const submitSetup = (e: FormEvent) => {
    e.preventDefault();
    if (pw !== pw2) {
      setErr("passwords do not match");
      return;
    }
    postAuth("/local/api/auth/setup", { username: user.trim(), password: pw });
  };

  const skipSetup = async () => {
    try {
      await fetch("/local/api/auth/skip", { method: "POST" });
    } catch {
      /* offline — still let the app try */
    }
    setLocked(false);
  };

  const submitLogin = (e: FormEvent) => {
    e.preventDefault();
    if (useKey) {
      const k = key.trim();
      if (!k) return;
      localStorage.setItem(LOCAL_KEY_STORAGE, k);
      postAuth("/local/api/auth/login", { api_key: k });
      return;
    }
    postAuth("/local/api/auth/login", { username: user.trim(), password: pw });
  };

  const shell = (inner: ReactNode) => (
    <div className="flex min-h-screen items-center justify-center p-6">
      <div className="card w-full max-w-sm space-y-4 p-6">
        <div>
          <h1 className="text-lg font-semibold">loom-client</h1>
          <p className="mt-1 text-sm text-muted">{inner ? undefined : null}</p>
        </div>
        {inner}
      </div>
    </div>
  );

  if (locked === null) {
    return (
      <div className="flex min-h-screen items-center justify-center text-sm text-muted">
        connecting…
      </div>
    );
  }

  if (locked && mode === "setup") {
    return shell(
      <>
        <p className="text-sm text-muted">
          First run — create a local admin user to protect this workstation's API,
          or keep it open for local use only.
        </p>
        <form className="space-y-3" onSubmit={submitSetup}>
          <input className="input w-full" placeholder="username" value={user} autoFocus onChange={(e) => setUser(e.target.value)} required />
          <input className="input w-full" type="password" placeholder="password (min 8 chars)" value={pw} onChange={(e) => setPw(e.target.value)} required minLength={8} />
          <input className="input w-full" type="password" placeholder="repeat password" value={pw2} onChange={(e) => setPw2(e.target.value)} required minLength={8} />
          {err && <p className="text-xs text-red-400">{err}</p>}
          <div className="flex items-center gap-2">
            <button className="btn-primary flex-1" type="submit" disabled={busy}>Create user</button>
            <button className="btn-ghost" type="button" onClick={skipSetup}>keep open</button>
          </div>
          <button className="w-full text-center text-xs text-muted hover:text-text" type="button" onClick={() => { setMode("login"); setUseKey(true); }}>
            sign in with API key instead
          </button>
        </form>
      </>
    );
  }

  if (locked) {
    return shell(
      <>
        <p className="text-sm text-muted">
          {useKey ? "Enter the local API key from your client config." : `Sign in${existingUser ? ` as ${existingUser}` : ""}.`}
        </p>
        <form className="space-y-3" onSubmit={submitLogin}>
          {useKey ? (
            <input className="input w-full" type="password" placeholder="local API key" value={key} autoFocus onChange={(e) => setKey(e.target.value)} required />
          ) : (
            <>
              <input className="input w-full" placeholder="username" value={user} autoFocus onChange={(e) => setUser(e.target.value)} required />
              <input className="input w-full" type="password" placeholder="password" value={pw} onChange={(e) => setPw(e.target.value)} required />
            </>
          )}
          {err && <p className="text-xs text-red-400">{err}</p>}
          <div className="flex items-center gap-2">
            <button className="btn-primary flex-1" type="submit" disabled={busy}>{useKey ? "Unlock" : "Sign in"}</button>
            <button className="btn-ghost" type="button" onClick={() => { setUseKey(!useKey); setErr(""); }}>
              {useKey ? "use password" : "use API key"}
            </button>
          </div>
        </form>
      </>
    );
  }

  return <>{children}</>;
}
