// Git credentials — the management surface for the keys a workspace clones with
// (MAIN-367).
//
// The API has been complete since `0001`: list, create (generate or paste), and
// delete. What was missing was any way to reach it. `NewWorkModal` offered a
// picker fed by `GET /api/v1/git-credentials`, but nothing in the product could
// CREATE one, so the picker was permanently empty and read as broken rather than
// as unpopulated.
//
// Modelled on GitHub's SSH keys page, because that is the shape people already
// know: a name they chose, the key's type, its SHA256 fingerprint, and when it
// was added.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type GitCredential } from "@nookos/api";
import { DataList, Empty, Panel, Pill, RowAction, RowActions, type DataColumn } from "@nookos/ui";
import { Copy, Plus, Trash2 } from "lucide-react";
import { askConfirm, askForm, notify } from "./dialogs";

/// The SHA256 fingerprint OpenSSH prints, computed here rather than served.
///
/// An OpenSSH public key is `<type> <base64 blob> [comment]`, and the
/// fingerprint is the base64 of the SHA-256 of the RAW BLOB — not of the text
/// line, which is the mistake that yields a plausible-looking value matching
/// nothing. Trailing `=` padding is stripped, as `ssh-keygen -l` does.
///
/// Computed client-side because the control plane stores the public half
/// verbatim and has no fingerprint column; deriving it here keeps this a display
/// concern rather than a migration.
export async function fingerprint(publicKey: string): Promise<string | null> {
  const blob = publicKey.trim().split(/\s+/)[1];
  if (!blob) return null;
  try {
    const raw = Uint8Array.from(atob(blob), (c) => c.charCodeAt(0));
    const digest = await crypto.subtle.digest("SHA-256", raw);
    const b64 = btoa(String.fromCharCode(...new Uint8Array(digest)));
    return `SHA256:${b64.replace(/=+$/, "")}`;
  } catch {
    // A key we cannot parse still lists — showing the row without a fingerprint
    // beats hiding a credential that exists and is selectable elsewhere.
    return null;
  }
}

/** `ssh-ed25519 AAAA… comment` → `ssh-ed25519`. */
function keyType(publicKey: string): string {
  return publicKey.trim().split(/\s+/)[0] || "ssh";
}

/// The list, with failure kept distinguishable from emptiness.
///
/// THROWS rather than `?? []`. Swallowing the error made react-query report
/// SUCCESS with an empty array, so a 403 or an unreachable control plane
/// rendered as "no credentials yet" — a claim the client had not established,
/// and the exact shape of the bug this whole ticket exists because of: a picker
/// that was empty for a reason nobody could see read as broken.
///
/// Shared by every caller (Settings, the workspace panel, + New Workspace) so
/// none of them can quietly reintroduce the swallow.
export async function fetchCredentials(): Promise<GitCredential[]> {
  const { data, error } = await api.GET("/api/v1/git-credentials");
  if (error) throw new Error(JSON.stringify(error));
  return data ?? [];
}

export function GitCredentials() {
  const queryClient = useQueryClient();
  const [prints, setPrints] = React.useState<Record<string, string>>({});

  const {
    data: creds,
    isLoading,
    isError,
  } = useQuery({
    queryKey: ["git-credentials"],
    queryFn: fetchCredentials,
  });

  // Fingerprints are async (Web Crypto), so they land after the rows do. Keyed
  // by credential id, which is why a re-render never recomputes a stable one.
  React.useEffect(() => {
    let live = true;
    (async () => {
      const next: Record<string, string> = {};
      for (const c of creds ?? []) {
        const fp = await fingerprint(c.public_key);
        if (fp) next[c.id] = fp;
      }
      if (live) setPrints(next);
    })();
    return () => {
      live = false;
    };
  }, [creds]);

  // One modal, two paths. Leaving the key box empty generates one here, which is
  // the path worth defaulting to: a key that never existed outside this control
  // plane cannot have leaked before it arrived.
  const add = async () => {
    const form = await askForm({
      title: "Add a git credential",
      description:
        "An ssh key a workspace can clone and fetch with. Leave the key box empty and one is generated for you.",
      fields: [
        {
          name: "name",
          label: "Name",
          placeholder: "deploy key — services repo",
          required: true,
        },
        {
          // Not `secret: true` — `dialogs.tsx` branches on `multiline` first, so
          // the flag never renders here and claiming otherwise is a lie about
          // what the screen does. Visible is right anyway: a PEM you are pasting
          // is one you already hold, and masking it only hides paste errors.
          name: "private_key",
          label: "Existing private key (optional)",
          placeholder: "-----BEGIN OPENSSH PRIVATE KEY-----",
          multiline: true,
        },
      ],
      confirmLabel: "add credential",
    });
    if (!form?.name?.trim()) return;

    const pasted = form.private_key?.trim() ?? "";
    const { data, error } = await api.POST("/api/v1/git-credentials", {
      body: {
        name: form.name.trim(),
        generate: pasted.length === 0,
        private_key: pasted.length > 0 ? pasted : null,
      },
    });
    if (error || !data) {
      await notify("Could not add the key", JSON.stringify(error));
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["git-credentials"] });
    // The PUBLIC half is the only part with somewhere to go, and this is the
    // moment to hand it over: a generated key is useless until the git host
    // trusts it. The private half never leaves the control plane except as
    // transient material delivered to a node for one git command.
    await notify(
      "Credential added — two steps left",
      "1. Add this public half to the repo on GitHub (Settings → Deploy keys), so the host trusts it.\n2. Pin this credential to the workspace that needs it — Workspaces → the repo → Git credential.",
      { copy: data.public_key },
    );
  };

  const remove = async (c: GitCredential) => {
    const ok = await askConfirm({
      title: `Delete "${c.name}"`,
      description:
        "Any workspace still pinning it must be unpinned first — the control plane refuses otherwise and names them, rather than letting a clone fail an hour later.",
      confirmLabel: "delete",
      danger: true,
    });
    if (!ok) return;
    const { error } = await api.DELETE("/api/v1/git-credentials/{id}", {
      params: { path: { id: c.id } },
    });
    if (error) {
      // The 409 body names the workspaces, which is the whole point of it.
      await notify(
        "Still in use",
        (error as { error?: string }).error ?? JSON.stringify(error),
      );
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["git-credentials"] });
  };

  const columns: DataColumn<GitCredential>[] = [
    {
      key: "name",
      header: "Name",
      className: "bright",
      cell: (c) => (
        <div>
          <div className="bright">{c.name}</div>
          <div className="mono muted" style={{ fontSize: "0.85em" }}>
            {prints[c.id] ?? "fingerprint unavailable"}
          </div>
        </div>
      ),
    },
    { key: "type", header: "Type", cell: (c) => <Pill>{keyType(c.public_key)}</Pill> },
    {
      key: "added",
      header: "Added",
      className: "muted",
      cell: (c) => `added ${new Date(c.created_at).toLocaleDateString()}`,
    },
    {
      key: "actions",
      header: "",
      cell: (c) => (
        <RowActions>
          <RowAction
            icon={Copy}
            title="copy the public half"
            onClick={() =>
              notify("Public key", "Add this to the repo as a deploy key.", {
                copy: c.public_key,
              })
            }
          />
          <RowAction icon={Trash2} danger title="delete this key" onClick={() => remove(c)} />
        </RowActions>
      ),
    },
  ];

  return (
    <Panel title="Git credentials">
      <div className="muted" style={{ marginBottom: "0.75rem" }}>
        SSH keys a workspace can clone and fetch with. Pin one to a workspace and every
        node that checks it out uses it — including operator nodes running loop work.
        A workspace pinning nothing falls back to the node's own generated key, which is
        what public repos have always used.
      </div>

      <button className="btn" onClick={add} style={{ marginBottom: "0.75rem" }}>
        <Plus size={14} /> add credential
      </button>

      {isLoading ? (
        <Empty>Loading credentials…</Empty>
      ) : isError ? (
        <Empty>
          Could not load credentials. This is not the same as having none — nothing has
          been added or removed; the list could not be read.
        </Empty>
      ) : (creds ?? []).length === 0 ? (
        <Empty>
          No credentials yet. Add one, put its public half on the git host, then pin it
          to the workspace that needs it.
        </Empty>
      ) : (
        <DataList rows={creds ?? []} columns={columns} rowKey={(c) => c.id} />
      )}
    </Panel>
  );
}
