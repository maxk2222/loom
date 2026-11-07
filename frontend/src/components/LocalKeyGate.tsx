import { FormEvent, useEffect, useState, type ReactNode } from "react";
import { LOCAL_KEY_STORAGE, NEED_KEY_EVENT, localKey } from "../lib/api";

/**
 * Local API key gate for the client UI. The workstation API requires a Bearer
 * key (generated into the client config on first run); once stored in
 * localStorage it is attached to every request. Any 401 re-opens the gate.
 */
export default function LocalKeyGate({ children }: { children: ReactNode }) {
  const [locked, setLocked] = useState<boolean | null>(null);
  const [key, setKey] = useState("");
  const [err, setErr] = useState("");

  const probe = async (): Promise<boolean> => {
    try {
      const headers: Record<string, string> = localKey()
        ? { authorization: `Bearer ${localKey()}` }
        : {};
      const res = await fetch("/local/api/status", { headers });
      const denied = res.status === 401;
      setLocked(denied);
      return !denied;
    } catch {
      // API unreachable — let the app render its own offline state.
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

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const k = key.trim();
    if (!k) return;
    localStorage.setItem(LOCAL_KEY_STORAGE, k);
    setKey("");
    if (!(await probe())) {
      setErr("Key rejected — check local_api.api_key in your client config");
    } else {
      setErr("");
    }
  };

  const forget = () => {
    localStorage.removeItem(LOCAL_KEY_STORAGE);
    setKey("");
    probe();
  };

  if (locked === null) {
    return (
      <div className="flex min-h-screen items-center justify-center text-sm text-muted">
        connecting…
      </div>
    );
  }
  if (locked) {
    return (
      <div className="flex min-h-screen items-center justify-center p-6">
        <form
          onSubmit={submit}
          className="card w-full max-w-sm space-y-4 p-6"
        >
          <div>
            <h1 className="text-lg font-semibold">loom-client</h1>
            <p className="mt-1 text-sm text-muted">
              Enter the local API key. Find it in your client config under{" "}
              <code className="text-acc">local_api.api_key</code> (e.g.{" "}
              <code className="text-acc">~/Library/Application&nbsp;Support/loom-client/config.toml</code>).
            </p>
          </div>
          <input
            className="input w-full"
            type="password"
            placeholder="local API key"
            value={key}
            autoFocus
            onChange={(e) => setKey(e.target.value)}
          />
          {err && <p className="text-xs text-red-400">{err}</p>}
          <div className="flex items-center gap-2">
            <button className="btn-primary flex-1" type="submit">
              Unlock
            </button>
            {localKey() && (
              <button className="btn-ghost" type="button" onClick={forget}>
                Reset
              </button>
            )}
          </div>
        </form>
      </div>
    );
  }
  return <>{children}</>;
}
