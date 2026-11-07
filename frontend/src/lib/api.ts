const VARIANT: string = (import.meta as any).env?.VITE_APP_VARIANT ?? (globalThis as any).__VARIANT__ ?? "server";
export const APP_VARIANT: "server" | "client" = VARIANT === "client" ? "client" : "server";

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    credentials: "include",
    headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
    ...init,
  });
  if (res.status === 401) {
    if (APP_VARIANT === "server" && !location.pathname.startsWith("/ui/login")) {
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
