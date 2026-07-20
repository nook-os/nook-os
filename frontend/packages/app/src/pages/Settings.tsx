import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@nookos/api";
import {
  applyTokens,
  DEFAULT_THEME,
  Empty,
  Panel,
  Pill,
  type ThemeTokens,
} from "@nookos/ui";

export function SettingsPage() {
  const queryClient = useQueryClient();
  const { data: themes } = useQuery({
    queryKey: ["themes"],
    queryFn: async () => (await api.GET("/api/v1/themes")).data ?? [],
  });
  const { data: settings } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });

  const activeTheme = String(
    (settings ?? []).find((s) => s.key === "theme")?.value ?? DEFAULT_THEME,
  );

  const pickTheme = async (slug: string, tokens: unknown) => {
    applyTokens(tokens as ThemeTokens);
    await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key: "theme" } },
      body: { value: slug, scope: "user" },
    });
    queryClient.invalidateQueries({ queryKey: ["settings"] });
  };

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr 1fr" }}>
      <Panel title="Theme">
        {(themes ?? []).length === 0 ? (
          <Empty>No themes installed.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(themes ?? []).map((t) => (
                <tr key={t.id}>
                  <td className="bright">{t.name}</td>
                  <td className="mono muted">{t.slug}</td>
                  <td>{t.tenant_id === null && <Pill>built-in</Pill>}</td>
                  <td>
                    {activeTheme === t.slug ? (
                      <Pill tone="ok">active</Pill>
                    ) : (
                      <button
                        className="btn small"
                        onClick={() => pickTheme(t.slug, t.tokens)}
                      >
                        use
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="Instance">
        <div style={{ padding: 10 }} className="small">
          <p className="muted">
            API docs: <a href="/docs" target="_blank" rel="noreferrer">/docs</a>
          </p>
          <p className="muted" style={{ marginTop: 8 }}>
            MCP endpoint: <span className="mono">/mcp</span> (bearer token from
            your instance config)
          </p>
          <p className="muted" style={{ marginTop: 8 }}>
            Add a machine: Nodes tab → “+ add node”.
          </p>
        </div>
      </Panel>
    </div>
  );
}
