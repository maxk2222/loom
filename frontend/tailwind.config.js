/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        bg: "#060a13",
        panel: "#0c1322",
        panel2: "#111a2c",
        border: "#1c2740",
        text: "#e2e8f0",
        muted: "#7d8da3",
        accent: "var(--accent, #4d8dff)",
        ok: "#34d399",
        warn: "#fbbf24",
        err: "#f87171",
      },
      boxShadow: {
        card: "0 1px 0 rgba(255,255,255,.03) inset, 0 10px 30px -12px rgba(0,0,0,.55)",
        glow: "0 0 24px -6px var(--accent-glow, rgba(77,141,255,.45))",
      },
    },
  },
  plugins: [],
};
