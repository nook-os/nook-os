// The user's app password: one passphrase that seals their secrets.
//
// Held in memory for the browser session only — never localStorage, never
// sent anywhere except the endpoints that already require it. Keeping it
// means auto-sync keeps working after the first unlock: when a new checkout
// appears we can re-push sealed secrets without asking again.
import { create } from "zustand";
import { api } from "@nookos/api";
import { askConfirm, askForm, askText, notify } from "./dialogs";
import { unlockWithPasskey } from "./passkey";

interface AppPasswordState {
  passphrase: string | null;
  set(passphrase: string): void;
  clear(): void;
}

export const useAppPassword = create<AppPasswordState>((set) => ({
  passphrase: null,
  set: (passphrase) => set({ passphrase }),
  clear: () => set({ passphrase: null }),
}));

export async function vaultConfigured(): Promise<boolean> {
  const { data } = await api.GET("/api/v1/vault/status", {});
  return !!data?.configured;
}

async function vaultStatus(): Promise<{ configured: boolean; passkeys: number }> {
  const { data } = await api.GET("/api/v1/vault/status", {});
  return { configured: !!data?.configured, passkeys: data?.passkeys ?? 0 };
}

/** Set the app password for the first time, with the warning it deserves. */
async function createAppPassword(): Promise<string | null> {
  const ok = await askConfirm({
    title: "Set your app password",
    description:
      "This one password encrypts every secret you store in NookOS.\n\n" +
      "• It is set ONCE and cannot be changed.\n" +
      "• NookOS never stores it — not even hashed in a way that could recover it.\n" +
      "• If you lose it, your stored secrets are unrecoverable.\n\n" +
      "Write it down somewhere safe before continuing.",
    confirmLabel: "I understand — set it",
  });
  if (!ok) return null;

  const out = await askForm({
    title: "App password",
    description: "At least 8 characters. There is no recovery and no reset.",
    fields: [
      { name: "passphrase", label: "App password", required: true },
      { name: "confirm", label: "Type it again", required: true },
    ],
    confirmLabel: "set password",
  });
  if (!out) return null;
  if (out.passphrase !== out.confirm) {
    await notify("Passwords don't match", "Nothing was saved — try again.");
    return null;
  }

  const { error, response } = await api.POST("/api/v1/vault/passphrase", {
    body: { passphrase: out.passphrase },
  });
  if (error || !response.ok) {
    await notify(
      "Could not set the app password",
      response.status === 409
        ? "One is already set — it cannot be changed."
        : JSON.stringify(error),
    );
    return null;
  }
  useAppPassword.getState().set(out.passphrase);
  return out.passphrase;
}

/** Ask for the existing app password and verify it before returning. */
async function promptForAppPassword(): Promise<string | null> {
  const passphrase = await askText({
    title: "Unlock secrets",
    description:
      "Your app password decrypts secrets for this browser session. NookOS cannot read them without it.",
    label: "App password",
    confirmLabel: "unlock",
  });
  if (!passphrase) return null;

  const { response } = await api.POST("/api/v1/vault/verify", {
    body: { passphrase },
  });
  if (!response.ok) {
    await notify("Wrong password", "That isn't your app password.");
    return null;
  }
  useAppPassword.getState().set(passphrase);
  return passphrase;
}

/**
 * The app password, asking for it however is appropriate.
 *
 * Order matters: already unlocked → passkey → typed password → first-time
 * setup. A passkey is both safer and less work than a password, so it goes
 * first whenever one is enrolled; declining or cancelling it falls straight
 * through to typing, because a lost phone must never mean a lost vault.
 */
export async function requireAppPassword(): Promise<string | null> {
  const held = useAppPassword.getState().passphrase;
  if (held) return held;

  const { configured, passkeys } = await vaultStatus();
  if (!configured) return createAppPassword();

  if (passkeys > 0) {
    const viaPasskey = await unlockWithPasskey();
    if (viaPasskey) {
      useAppPassword.getState().set(viaPasskey);
      return viaPasskey;
    }
  }
  return promptForAppPassword();
}
