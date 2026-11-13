import { clsx } from "clsx";
import {
  Boxes,
  Route as RouteIcon,
  Cpu,
  Gauge,
  Home,
  Key,
  Layers,
  LogOut,
  Megaphone,
  MonitorCog,
  Menu,
  UserCog,
  ScrollText,
  Settings as SettingsIcon,
  Share2,
  Users,
} from "lucide-react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { api, APP_VARIANT } from "../lib/api";
import { useBranding } from "../lib/branding";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Badge, Logo } from "./ui";

interface NavItem {
  to: string;
  label: string;
  icon: React.ReactNode;
}

const SERVER_NAV: NavItem[] = [
  { to: "/", label: "Overview", icon: <Home size={16} /> },
  { to: "/clients", label: "Clients", icon: <Cpu size={16} /> },
  { to: "/notes", label: "Notices", icon: <Megaphone size={16} /> },
  { to: "/models", label: "Models", icon: <Boxes size={16} /> },
  { to: "/routers", label: "Auto Router", icon: <RouteIcon size={16} /> },
  { to: "/deployments", label: "Deployments", icon: <Layers size={16} /> },
  { to: "/scheduler", label: "Scheduler", icon: <Gauge size={16} /> },
  { to: "/api-users", label: "API Users", icon: <UserCog size={16} /> },
  { to: "/keys", label: "API Keys", icon: <Key size={16} /> },
  { to: "/users", label: "Users", icon: <Users size={16} /> },
  { to: "/monitoring", label: "Monitoring", icon: <ScrollText size={16} /> },
  { to: "/settings", label: "Settings", icon: <SettingsIcon size={16} /> },
];

const CLIENT_NAV: NavItem[] = [
  { to: "/", label: "Status", icon: <Home size={16} /> },
  { to: "/backends", label: "Backends", icon: <Cpu size={16} /> },
  { to: "/sharing", label: "Sharing", icon: <Share2 size={16} /> },
  { to: "/requests", label: "Requests", icon: <MonitorCog size={16} /> },
  { to: "/connection", label: "Connection", icon: <Gauge size={16} /> },
];

function UpdateChip() {
  const [res, setRes] = useState<any>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const isClient = APP_VARIANT === "client";
  const check = async () =>
    setRes(await api.get<any>(isClient ? "/local/api/update-check" : "/api/update-check").catch(() => null));
  const apply = async () => {
    setBusy(true);
    setErr(null);
    const r = await api
      .post<any>(isClient ? "/local/api/update" : "/api/server/update")
      .catch(() => null);
    if (!r || r.ok === false) {
      setErr(String(r?.error || "update failed"));
      setBusy(false);
      return;
    }
    // Server path: `restarting:true` means the process exits now — poll the
    // status endpoint until it answers again, then drop the busy state.
    if (!isClient && r?.restarting) {
      for (let i = 0; i < 30; i++) {
        await new Promise((res) => setTimeout(res, 2000));
        const st = await api.get<any>("/api/server/update-status").catch(() => null);
        if (st?.version) {
          setBusy(false);
          setRes(null);
          return;
        }
      }
      setErr("server did not come back — check logs");
    }
    setBusy(false);
  };
  if (!res)
    return <button className="btn-ghost px-2 py-0.5 text-[11px]" onClick={check}>check update</button>;
  const newer = res.latest && res.latest.replace(/^v/, "") !== res.current;
  return (
    <span className="inline-flex items-center gap-1.5">
      {newer ? (
        <>
          <a className="badge bg-warn/12 text-warn" href={res.url} target="_blank" rel="noreferrer">
            ⬆ {res.latest}
          </a>
          <button className="btn-ghost px-1.5 py-0.5 text-[10px]" disabled={busy} onClick={apply}
            title="Apply update now (downloads, verifies and restarts)">
            {busy ? "…" : "apply"}
          </button>
        </>
      ) : (
        <Badge tone="ok">up to date</Badge>
      )}
      {err && <span className="text-[10px] text-err" title={err}>failed</span>}
    </span>
  );
}

function StatusBar() {
  const isClient = APP_VARIANT === "client";
  const q = useQuery({
    queryKey: [isClient ? "client-status" : "overview"],
    queryFn: () =>
      api.get<any>(isClient ? "/local/api/status" : "/api/overview"),
    refetchInterval: 8000,
  });
  const d = q.data ?? {};
  const connected = isClient ? d.server_connected : true;
  // In-flight counters live per deployment under capacities[] — sum them.
  const infl = (d.capacities ?? []).reduce(
    (a: [number, number], c: any) => [a[0] + (c.inflight_local ?? 0), a[1] + (c.inflight_tier2 ?? 0)],
    [0, 0],
  );
  return (
    <div className="mt-auto space-y-2 border-t border-border/60 px-4 py-3 text-[11px]">
      <Badge tone={connected ? "ok" : "err"} dot>
        {isClient ? (connected ? "server" : "offline") : "server live"}
      </Badge>{" "}
      {isClient ? (
        <Badge tone="muted">l{infl[0]} · t{infl[1]}</Badge>
      ) : (
        <Badge tone="muted">{d.clients_online ?? 0}/{d.clients_total ?? 0} clients</Badge>
      )}
      <div className="flex items-center justify-between pt-1">
        <UpdateChip />
        <span className="text-muted/50">{isClient ? "client" : "control"}</span>
      </div>
    </div>
  );
}

export default function Layout() {
  const brand = useBranding();
  const nav = useNavigate();
  const [drawer, setDrawer] = useState(false);
  const items = APP_VARIANT === "client" ? CLIENT_NAV : SERVER_NAV;

  const logout = async () => {
    try {
      await api.post("/api/auth/logout");
    } catch {
      /* ignore */
    }
    nav("/login");
  };

  const sidebar = (
    <>
      <div className="grad-line" />
      <div className="flex items-center gap-2.5 px-4 pb-4 pt-4">
        <Logo size={30} />
        <div className="min-w-0">
          <div className="grad-text text-sm font-bold leading-tight tracking-wide">
            {brand.app_name}
          </div>
          <div className="truncate text-[10px] leading-tight text-muted">
            {APP_VARIANT === "client" ? "Workstation Client" : "Server Dashboard"}
          </div>
        </div>
      </div>
      <nav className="flex-1 space-y-0.5 overflow-y-auto px-2.5" onClick={() => setDrawer(false)}>
        {items.map((it) => (
          <NavLink
            key={it.to}
            to={it.to}
            end={it.to === "/"}
            className={({ isActive }) =>
              clsx(
                "group relative flex items-center gap-2.5 rounded-lg px-3 py-2 text-sm transition-all",
                isActive
                  ? "bg-accent/12 text-text"
                  : "text-muted hover:bg-panel2 hover:text-text",
              )
            }
          >
            {({ isActive }) => (
              <>
                <span
                  className={clsx(
                    "absolute left-0 top-1/2 h-4 w-0.5 -translate-y-1/2 rounded-full transition-all",
                    isActive ? "bg-accent opacity-100" : "opacity-0",
                  )}
                  style={isActive ? { background: "var(--grad)" } : undefined}
                />
                <span className={isActive ? "text-accent" : "text-muted/70 group-hover:text-muted"}>
                  {it.icon}
                </span>
                {it.label}
              </>
            )}
          </NavLink>
        ))}
      </nav>
      <StatusBar />
      <div className="space-y-1 border-t border-border p-2.5">
        {APP_VARIANT === "server" && (
          <button onClick={logout} className="btn-ghost w-full justify-start px-3">
            <LogOut size={16} /> Sign out
          </button>
        )}
        <VersionTag />
      </div>
    </>
  );

  return (
    <div className="flex min-h-screen">
      {/* Desktop sidebar */}
      <aside className="sticky top-0 hidden h-screen w-56 shrink-0 flex-col border-r border-border bg-panel/60 backdrop-blur md:flex">
        {sidebar}
      </aside>
      {/* Mobile drawer */}
      {drawer && (
        <div className="fixed inset-0 z-40 md:hidden">
          <div className="absolute inset-0 bg-black/60 backdrop-blur-sm" onClick={() => setDrawer(false)} />
          <aside className="page-enter absolute left-0 top-0 flex h-full w-64 flex-col border-r border-border bg-panel">
            {sidebar}
          </aside>
        </div>
      )}
      <div className="flex min-w-0 flex-1 flex-col">
        {/* Mobile top bar */}
        <header className="sticky top-0 z-30 flex items-center gap-3 border-b border-border bg-bg/80 px-4 py-3 backdrop-blur md:hidden">
          <button className="btn-ghost p-1.5" onClick={() => setDrawer(true)} aria-label="Open menu">
            <Menu size={18} />
          </button>
          <Logo size={22} />
          <span className="grad-text text-sm font-bold tracking-wide">{brand.app_name}</span>
        </header>
        <main className="min-w-0 flex-1 p-4 sm:p-6 lg:p-8">
        <div className="page-enter mx-auto w-full max-w-6xl">
          <Outlet />
        </div>
        </main>
      </div>
    </div>
  );
}

function VersionTag() {
  const v = useQuery({
    queryKey: ["loom-version"],
    queryFn: () =>
      APP_VARIANT === "client"
        ? api.get<{ version?: string }>("/local/api/status").then(d => d.version)
        : api.get<{ version?: string }>("/api/system").then(d => d.version),
    refetchInterval: 60000,
  });
  return (
    <div className="px-3 pb-1 pt-1 text-[10px] text-muted/50">
      {APP_VARIANT === "client" ? "loom-client" : "loom-server"}
      {v.data ? ` v${v.data}` : ""}
    </div>
  );
}
