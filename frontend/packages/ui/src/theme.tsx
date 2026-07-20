// Theme engine: themes are token JSON from the control plane, applied as CSS
// custom properties. Every visual aspect is configurable; unknown token keys
// pass straight through as --nook-<section>-<key> variables.
import React, { createContext, useContext, useEffect, useState } from "react";
import { api, type Theme } from "@nookos/api";

export interface ThemeTokens {
  colors?: Record<string, string>;
  fonts?: Record<string, string>;
  spacing?: Record<string, string>;
  effects?: Record<string, string>;
}

const ThemeContext = createContext<{ theme: Theme | null; tokens: ThemeTokens }>({
  theme: null,
  tokens: {},
});

const VAR_MAP: Record<string, string> = {
  "colors.bg": "--nook-bg",
  "colors.bg-panel": "--nook-bg-panel",
  "colors.bg-raised": "--nook-bg-raised",
  "colors.fg": "--nook-fg",
  "colors.fg-bright": "--nook-fg-bright",
  "colors.fg-dim": "--nook-fg-dim",
  "colors.fg-faint": "--nook-fg-faint",
  "colors.accent": "--nook-accent",
  "colors.border": "--nook-border",
  "colors.border-bright": "--nook-border-bright",
  "colors.ok": "--nook-ok",
  "colors.warn": "--nook-warn",
  "colors.err": "--nook-err",
  "colors.info": "--nook-info",
  "colors.selection": "--nook-selection",
  "colors.terminal-bg": "--nook-terminal-bg",
  "colors.terminal-cursor": "--nook-terminal-cursor",
  "fonts.mono": "--nook-font-mono",
  "fonts.ui": "--nook-font-ui",
  "spacing.unit": "--nook-unit",
  "spacing.panel-gap": "--nook-panel-gap",
  "spacing.radius": "--nook-radius",
  "effects.glow": "--nook-glow",
  "effects.glow-strong": "--nook-glow-strong",
};

export function applyTokens(tokens: ThemeTokens) {
  const root = document.documentElement;
  for (const [section, values] of Object.entries(tokens) as [
    string,
    Record<string, string> | undefined,
  ][]) {
    if (!values) continue;
    for (const [key, value] of Object.entries(values)) {
      const mapped = VAR_MAP[`${section}.${key}`] ?? `--nook-${section}-${key}`;
      root.style.setProperty(mapped, value);
    }
  }
  document.body.classList.toggle(
    "scanlines",
    tokens.effects?.scanlines === "on",
  );
}

/** xterm.js theme derived from the active tokens. */
export function terminalTheme(tokens: ThemeTokens) {
  const c = tokens.colors ?? {};
  return {
    background: c["terminal-bg"] ?? c.bg ?? "#0a0705",
    foreground: c.fg ?? "#ffb000",
    cursor: c["terminal-cursor"] ?? c.accent ?? "#ffb000",
    selectionBackground: c.selection ?? "#3a2c0c",
  };
}

export const DEFAULT_THEME = "charcoal-gold";

export function ThemeProvider({
  slug,
  children,
}: {
  slug?: string;
  children: React.ReactNode;
}) {
  const [theme, setTheme] = useState<Theme | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      // Saved preference wins; fall back to the instance default theme.
      let chosen = slug;
      if (!chosen) {
        try {
          const { data } = await api.GET("/api/v1/settings");
          const saved = data?.find((s) => s.key === "theme")?.value;
          if (typeof saved === "string" && saved) chosen = saved;
        } catch {
          // not signed in yet — default is fine
        }
      }
      try {
        const { data } = await api.GET("/api/v1/themes/{slug}", {
          params: { path: { slug: chosen ?? DEFAULT_THEME } },
        });
        if (!cancelled && data) {
          setTheme(data);
          applyTokens(data.tokens as ThemeTokens);
        }
      } catch {
        // Defaults in global.css keep the app usable.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [slug]);

  return (
    <ThemeContext.Provider
      value={{ theme, tokens: (theme?.tokens as ThemeTokens) ?? {} }}
    >
      {children}
    </ThemeContext.Provider>
  );
}

export function useTheme() {
  return useContext(ThemeContext);
}
