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
import { askConfirm, notify } from "./dialogs";

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

export function GitCredentials() {
  const queryClient = useQueryClient();
  const [name, setName] = React.useState("");
  const [privateKey, setPrivateKey] = React.useState("");
  const [pasting, setPasting] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [prints, setPrints] = React.useState<Record<string, string>>({});

  const { data: creds } = useQuery({
    queryKey: ["git-credentials"],
    queryFn: async () => (await api.GET("/api/v1/git-credentials")).data ?? [],
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

  const create = async () => {
    if (!name.trim()) {
      await notify("Name it first", "The name is how you tell keys apart later.");
      return;
    }
    setBusy(true);
    const { data, error } = await api.POST("/api/v1/git-credentials", {
      body: {
        name: name.trim(),
        generate: !pasting,
        private_key: pasting ? privateKey : null,
      },
    });
    setBusy(false);
    if (error || !data) {
      await notify("Could not add the key", JSON.stringify(error));
      return;
    }
    setName("");
    setPrivateKey("");
    setPasting(false);
    queryClient.invalidateQueries({ queryKey: ["git-credentials"] });
    // The PUBLIC half is the thing with somewhere to go: it has to be added to
    // the repo as a deploy key before any of this works. Offering it at the
    // moment of creation is the difference between a key that works and one
    // that sits here unused.
    await notify(
      "Key added — now authorize it on the repo",
      "Add this public half as a deploy key (or to your git account). The private half stays sealed in the control plane and is only ever delivered to a node for a single git command.",
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

      {(creds ?? []).length === 0 ? (
        <Empty>No keys yet. Generate one below, then add its public half to the repo.</Empty>
      ) : (
        <DataList rows={creds ?? []} columns={columns} rowKey={(c) => c.id} />
      )}

      <div className="field" style={{ marginTop: "1rem" }}>
        <label>Add a key</label>
        <input
          className="input"
          value={name}
          placeholder="deploy key — services repo"
          onChange={(e) => setName(e.target.value)}
        />
      </div>

      {pasting && (
        <div className="field">
          <label>Private key (PEM)</label>
          <textarea
            className="input mono"
            rows={6}
            value={privateKey}
            placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
            onChange={(e) => setPrivateKey(e.target.value)}
          />
          <div className="muted" style={{ fontSize: "0.85em" }}>
            Sealed with the tenant vault on arrival. Prefer generating instead — a key
            that never existed outside this control plane cannot have leaked before it
            got here.
          </div>
        </div>
      )}

      <RowActions>
        <button className="btn" disabled={busy} onClick={create}>
          <Plus size={14} /> {pasting ? "add pasted key" : "generate key"}
        </button>
        <button className="btn ghost" disabled={busy} onClick={() => setPasting((p) => !p)}>
          {pasting ? "generate one instead" : "paste an existing key"}
        </button>
      </RowActions>
    </Panel>
  );
}
