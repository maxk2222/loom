const VARIANT: string = (import.meta as any).env?.VITE_APP_VARIANT ?? (globalThis as any).__VARIANT__ ?? "server";
export const APP_VARIANT: "server" | "client" = VARIANT === "client" ? "client" : "server";

export const LOCAL_KEY_STORAGE = "loom_local_key";
export const NEED_KEY_EVENT = "loom-need-key";

export function localKey(): string {
  return localStorage.getItem(LOCAL_KEY_STORAGE) ?? "";
}

/** Clipboard copy that also works over plain-HTTP LAN access, where
 * navigator.clipboard is unavailable (non-secure context). */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* fall through to execCommand */
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const auth: Record<string, string> =
    APP_VARIANT === "client" && localKey()
      ? { authorization: `Bearer ${localKey()}` }
      : {};
  const res = await fetch(path, {
    credentials: "include",
    headers: { "content-type": "application/json", ...auth, ...(init?.headers ?? {}) },
    ...init,
  });
  if (res.status === 401) {
    if (APP_VARIANT === "client") {
      window.dispatchEvent(new Event(NEED_KEY_EVENT));
    } else if (!location.pathname.startsWith("/ui/login")) {
      location.assign("/ui/login");
    }
    throw new Error("unauthorized");
  }
  const text = await res.text();
  const body = text ? JSON.parse(text) : {};
  if (!res.ok) {
    throw new Error(body?.error ?? res.statusText);
  }
  return body as T;
}

const j = (body: unknown) => ({ body: JSON.stringify(body) });

export const api = {
  get: <T>(p: string) => req<T>(p),
  post: <T>(p: string, b?: unknown) => req<T>(p, { method: "POST", ...j(b ?? {}) }),
  put: <T>(p: string, b?: unknown) => req<T>(p, { method: "PUT", ...j(b ?? {}) }),
  del: <T>(p: string) => req<T>(p, { method: "DELETE" }),
  postForm: async <T>(p: string, f: FormData): Promise<T> => {
    const res = await fetch(p, { method: "POST", body: f, credentials: "include" });
    if (!res.ok) throw new Error((await res.text()) || res.statusText);
    return res.json() as Promise<T>;
  },
};

export function openEventStream(onEvent: (kind: string, data: unknown) => void): () => void {
  const es = new EventSource("/api/events");
  const kinds = ["overview", "clients", "queue", "branding", "requests", "deployments"];
  for (const k of kinds) {
    es.addEventListener(k, (e) => {
      try {
        onEvent(k, JSON.parse((e as MessageEvent).data));
      } catch {
        /* ignore */
      }
    });
  }
  return () => es.close();
}
