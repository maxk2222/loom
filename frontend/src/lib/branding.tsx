import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { api } from "./api";
import type { Branding } from "./types";

const DEFAULTS: Branding = {
  app_name: "LOOM",
  tagline: "LLM Orchestration & Operations Mesh",
  logo_path: "",
  accent_color: "#5b8def",
};

const BrandingCtx = createContext<Branding>(DEFAULTS);

export function useBranding(): Branding {
  return useContext(BrandingCtx);
}

export function BrandingProvider({ children }: { children: ReactNode }) {
  const [b, setB] = useState<Branding>(DEFAULTS);

  useEffect(() => {
    api
      .get<Branding>("/api/branding")
      .then((v) => setB({ ...DEFAULTS, ...v }))
      .catch(() => {});
  }, []);

  useEffect(() => {
    document.title = b.app_name || "LOOM";
    document.documentElement.style.setProperty("--accent", b.accent_color || DEFAULTS.accent_color);
  }, [b.app_name, b.accent_color]);

  return <BrandingCtx.Provider value={b}>{children}</BrandingCtx.Provider>;
}
