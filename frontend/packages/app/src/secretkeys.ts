// Passphrases held for the browser session only.
//
// Sealed secrets can't ride the control plane's automatic sync — the server
// can't read them. Rather than turn auto-sync off, we keep the passphrase in
// memory after you unlock a workspace and replay the unlock whenever a new
// checkout appears, which is what actually pushes the file. Enter it once,
// auto-sync keeps working.
//
// Memory only: never localStorage, never sent anywhere except the unlock
// endpoint that already required it.
import { create } from "zustand";

interface SecretKeysState {
  /** workspace id → passphrase, for this tab, until reload. */
  keys: Record<string, string>;
  remember(workspaceId: string, passphrase: string): void;
  forget(workspaceId: string): void;
  forgetAll(): void;
}

export const useSecretKeys = create<SecretKeysState>((set) => ({
  keys: {},
  remember: (workspaceId, passphrase) =>
    set((s) => ({ keys: { ...s.keys, [workspaceId]: passphrase } })),
  forget: (workspaceId) =>
    set((s) => {
      const keys = { ...s.keys };
      delete keys[workspaceId];
      return { keys };
    }),
  forgetAll: () => set({ keys: {} }),
}));

/** Re-push a workspace's sealed secrets to its checkouts, if we hold the key. */
export async function resyncSealedSecrets(
  workspaceId: string,
  api: {
    GET: (path: string, opts: unknown) => Promise<{ data?: unknown }>;
    POST: (path: string, opts: unknown) => Promise<{ error?: unknown }>;
  },
): Promise<number> {
  const passphrase = useSecretKeys.getState().keys[workspaceId];
  if (!passphrase) return 0;

  const { data } = await api.GET("/api/v1/workspaces/{id}/secrets", {
    params: { path: { id: workspaceId } },
  });
  const secrets = (data ?? []) as { name: string; protected?: boolean }[];
  let synced = 0;
  for (const s of secrets.filter((x) => x.protected)) {
    // Unlocking is what pushes the plaintext to every online checkout.
    const { error } = await api.POST(
      "/api/v1/workspaces/{id}/secrets/{name}/open",
      {
        params: { path: { id: workspaceId, name: s.name } },
        body: { passphrase },
      },
    );
    if (!error) synced += 1;
  }
  return synced;
}
