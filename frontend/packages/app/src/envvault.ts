// One door for every .env operation.
//
// A .env is the one thing NookOS moves between machines that would really
// hurt to leak, so there is no path that stores one without the app password:
// saving, importing a repo that already has one, adopting one found on disk.
// Every caller goes through here rather than reaching for the endpoint, which
// is how the "just this once, unsealed" cases crept in before.
import { api } from "@nookos/api";
import { requireAppPassword } from "./apppassword";
import { askConfirm, notify } from "./dialogs";

/** Seal content under the app password and sync it to every online checkout. */
export async function saveEnv(
  workspaceId: string,
  content: string,
  opts: { ephemeral?: boolean; name?: string } = {},
): Promise<boolean> {
  const passphrase = await requireAppPassword();
  if (!passphrase) return false; // backed out of setting/entering one

  const name = opts.name ?? ".env";
  const { error, response } = await api.PUT("/api/v1/workspaces/{id}/secrets/{name}", {
    params: { path: { id: workspaceId, name } },
    body: { content, passphrase, ephemeral: opts.ephemeral ?? false },
  });
  if (error || !response.ok) {
    await notify(
      response.status === 403 ? "Wrong app password" : `Could not save ${name}`,
      response.status === 403
        ? "That isn't your app password."
        : JSON.stringify(error),
    );
    return false;
  }
  return true;
}

/**
 * A freshly imported repo often already has a .env sitting in it. Offer to
 * take it into the vault, which is what makes it encrypted and what lets it
 * follow the workspace to another machine. Silent when there's nothing to
 * adopt — this runs after every import, and an interruption should mean
 * something.
 */
export async function adoptEnvFromDisk(
  workspaceId: string,
  name = ".env",
): Promise<boolean> {
  const { data } = await api.GET("/api/v1/workspaces/{id}/secrets/{name}/on-disk", {
    params: { path: { id: workspaceId, name } },
  });
  if (!data?.found || data.in_vault) return false;

  const ok = await askConfirm({
    title: `This repo came with a ${name}`,
    description:
      `Found ${name} in ${data.checkout_path ?? "the checkout"}.\n\n` +
      "Take it into the vault? It gets encrypted with your app password, and " +
      "every other checkout of this workspace gets a copy — that's how it " +
      "travels with you.\n\n" +
      "Left alone, it stays on that one machine, unencrypted.",
    confirmLabel: "encrypt & import",
  });
  if (!ok) return false;

  // Asked after the confirm, so someone declining never gets a password prompt
  // for something they didn't want.
  const passphrase = await requireAppPassword();
  if (!passphrase) return false;

  const { data: result, error, response } = await api.POST(
    "/api/v1/workspaces/{id}/secrets/{name}/import",
    {
      params: { path: { id: workspaceId, name } },
      body: { passphrase, ephemeral: false },
    },
  );
  if (error || !response.ok) {
    await notify(
      response.status === 403 ? "Wrong app password" : `Could not import ${name}`,
      response.status === 403
        ? "That isn't your app password."
        : JSON.stringify(error),
    );
    return false;
  }
  if (!result?.ok) {
    await notify(`Nothing to import`, result?.message ?? "");
    return false;
  }
  return true;
}
