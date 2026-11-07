/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        bg: "#0b0f14",
        panel: "#10161d",
        panel2: "#151d26",
        border: "#1f2a36",
        text: "#dbe4ee",
        muted: "#8b9bad",
        accent: "var(--accent, #5b8def)",
        ok: "#3fb96c",
        warn: "#d9a63f",
        err: "#e05d5d",
      },
    },
  },
  plugins: [],
};
