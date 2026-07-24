# Spec — end-to-end encrypted workspace secrets

Status: **proposed** (not implemented). Supersedes the current server-side
vault when accepted.

> **Not the same "secrets" as deploying the control plane.** This doc is about
> the **application sealed secrets** NookOS stores for tenants (the workspace
> `.env` vault). The credentials that get the control plane *running* on
> Kubernetes — `DATABASE_URL`, `SESSION_SECRET`, and `SECRETS_KEY` itself — are
> **deployment credentials**, supplied via a referenced Kubernetes Secret and
> optionally synced from Vault/GCP/AWS; see
> [`charts/nook-control/examples/secrets/`](../charts/nook-control/examples/secrets/README.md).
> `SECRETS_KEY` is the bridge: a deployment credential that unlocks the vault
> described here.

## Why

Today `.env` contents are encrypted at rest with `SECRETS_KEY` (AES-256-GCM,
`crypto.rs`) and decrypted by the control plane whenever a checkout needs the
file. That protects against a bare database dump and nothing else:

- **The operator can read every tenant's secrets.** Anyone with shell access
  to the control plane can decrypt the whole vault. That's a liability the
  operator should not *want* — "I can't read your secrets" is a stronger and
  more honest position than "I promise not to".
- **DB leak + app key leak = total compromise.** Both live on the same host
  and are frequently captured together (backup with env file, container image
  with mounted secret, CI artifact). One breach wrecks every repo of every
  tenant: cloud keys, database URLs, signing keys.

The goal: **a full database dump plus `SECRETS_KEY` must be worthless.**
Decryption must additionally require something the server never stores.

## Threat model

| Attacker capability | Today | Proposed |
| --- | --- | --- |
| Database dump only | safe | safe |
| Database dump + `SECRETS_KEY` | **total compromise** | safe |
| Malicious/compelled operator (root on control plane) | **reads everything** | safe at rest¹ |
| Compromised node | reads that node's checkouts | reads that node's checkouts (unchanged) |
| User's passphrase phished | that user's secrets | that user's secrets |
| Stolen laptop with browser session | reads secrets | needs the passphrase too |

¹ An actively malicious operator can still ship modified frontend JavaScript
to capture a passphrase as it's typed. That is inherent to browser-delivered
crypto and must be stated plainly rather than papered over — see
[Residual risks](#residual-risks). The design defends against *offline*
compromise (dumps, backups, subpoenas of data at rest), which is the realistic
threat.

## Design: envelope encryption, keys never on the server

Three layers, each independently necessary:

```
                    ┌──────────────────────────────────────────┐
 passphrase ──KDF──▶│ KEK (key-encryption key)  browser only   │
                    └───────────────────┬──────────────────────┘
                                        │ wraps
                    ┌───────────────────▼──────────────────────┐
      random ──────▶│ DEK (per-secret data key)  browser only  │
                    └───────────────────┬──────────────────────┘
                                        │ encrypts
                    ┌───────────────────▼──────────────────────┐
   .env plaintext ─▶│ ciphertext            stored on server   │
                    └──────────────────────────────────────────┘
```

The server stores `{salt, wrapped_dek[], ciphertext}` and additionally wraps
that record with `SECRETS_KEY` at rest (defense in depth, unchanged from
today). It never holds the passphrase, the KEK, the DEK, or the plaintext.

### Key derivation (browser)

```
KEK = Argon2id(passphrase, salt = per-tenant random 16B,
               m = 64 MiB, t = 3, p = 1) -> 32 bytes
```

Argon2id with memory-hard parameters so an offline dictionary attack on a
leaked `salt + wrapped_dek` stays expensive. Parameters are stored alongside
the salt so they can be raised later without breaking old records.

### Per-secret encryption (browser)

```
DEK        = CSPRNG(32)
nonce      = CSPRNG(24)
ciphertext = XChaCha20-Poly1305(DEK, nonce, plaintext,
                                aad = tenant_id ‖ workspace_id ‖ name)
```

XChaCha20-Poly1305 for its 192-bit nonce (random nonces are safe without a
counter) and constant-time software implementation. The AAD binds a secret to
its workspace and filename, so a stolen blob can't be replayed into a
different workspace to trick a node into writing it somewhere else.

### Wrapping the DEK

The same DEK is wrapped once per recipient. Recipients are:

1. **The user's passphrase KEK** — `XChaCha20-Poly1305(KEK, …, DEK)`.
   One wrap per user who may unlock the workspace (each with their own salt),
   so access can be granted and revoked per person without re-encrypting.
2. **Each authorized node** — sealed to the node's public key
   (X25519 ECDH → HKDF-SHA256 → XChaCha20-Poly1305). Nodes already generate
   an Ed25519 identity key at join (`ssh.rs`); this adds an X25519 key
   alongside it, reported like the SSH public key.

That second recipient is what makes the sync path work **without the server
ever seeing plaintext**: the node unwraps the DEK with its own private key,
decrypts, and writes the 0600 file. The control plane is a courier.

### Data flow

**Save** (browser): unlock → derive KEK → generate DEK → encrypt →
wrap DEK for self + every authorized node → `PUT` the blob. Server stores it
(wrapped again with the app key) and pushes to online checkouts.

**Sync** (node): receives `{ciphertext, wrapped_dek_for_me}` → unwraps DEK
with its X25519 private key → decrypts → writes `.env` mode 0600 → zeroizes
the DEK.

**Read in UI** (browser): fetch blob → unwrap DEK with KEK → decrypt →
display. Cleared from memory on lock.

### Schema

Replaces `workspace_secrets.content_enc`:

```sql
workspace_secrets(
  id, tenant_id, workspace_id, name,
  ciphertext      BYTEA NOT NULL,   -- nonce ‖ XChaCha20-Poly1305 output
  aad_version     INT   NOT NULL,
  created_at, updated_at
)
secret_recipients(
  secret_id  REFERENCES workspace_secrets ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('user','node','recovery')),
  subject_id UUID NOT NULL,         -- user_id | node_id | recovery_code_id
  wrapped_dek BYTEA NOT NULL,
  PRIMARY KEY (secret_id, kind, subject_id)
)
tenant_kdf(tenant_id PRIMARY KEY, salt BYTEA, algo TEXT, params JSONB)
node_keys(node_id PRIMARY KEY, x25519_public BYTEA, created_at)
```

Every stored `BYTEA` is additionally app-key-wrapped by `crypto.rs` on write.

## Key management UX

- **Unlock**: one passphrase prompt per browser session, held in memory only
  (never `localStorage`). Auto-locks after inactivity.
- **Recovery**: at setup the user gets a random 256-bit recovery code
  (BIP39-style words) — a third recipient wrap. Lose the passphrase without
  it and the data is gone; say so loudly at setup, twice.
- **Optional escrow**: a tenant may explicitly opt in to wrapping for an
  admin's key. Off by default, visible in the UI when on — the operator can
  never silently hold a copy.
- **Adding a node**: the node's X25519 key arrives at join; every existing
  secret must be re-wrapped for it. Requires an unlocked browser session —
  the UI prompts "3 secrets need to be shared with node *azul*".
- **Revoking a node**: drop its `secret_recipients` rows. Assume anything it
  already synced is compromised → rotate those values at the source.
- **Rotating the passphrase**: re-wrap DEKs only (cheap); ciphertexts are
  untouched.

## Migration

1. Ship the schema and the browser crypto with the current server-side vault
   still working — existing secrets keep `content_enc`.
2. On first unlock, the UI offers "upgrade this workspace to end-to-end
   encryption": read via the old path, re-encrypt client-side, write the new
   rows, delete `content_enc`.
3. Once a tenant has no `content_enc` rows, the server-side decrypt path is
   dead for them. Remove it entirely after all tenants migrate.

MCP/API note: `read_secret`-style endpoints necessarily return only
ciphertext after migration. Automation that needs plaintext must hold a
recipient key — i.e. run on a node, or be given its own wrapped DEK.

## Residual risks

- **Malicious frontend delivery.** A compromised control plane can serve
  JavaScript that exfiltrates the passphrase. Mitigations: Subresource
  Integrity, a published build hash users can verify, and (best) doing the
  crypto in the Tauri desktop app where the binary is signed and updated out
  of band. Document this honestly; do not claim protection we don't have.
- **Plaintext on the node.** `.env` lands on disk by design — that's the
  point. Node compromise = those secrets. Unchanged, and orthogonal.
- **Metadata.** Names (`.env`), sizes, and timestamps stay visible to the
  server. Acceptable; encrypting them buys little and breaks listing.
- **Passphrase loss.** Real data loss. Recovery codes are the answer, and
  they must be impossible to skip past accidentally.

## Why not alternatives

- **Per-tenant server-side keys**: still operator-readable. Doesn't meet the
  bar.
- **KMS/HSM (Vault, cloud KMS)**: strong against DB leak, but the control
  plane still asks for plaintext, so the operator still sees it — plus it
  adds infrastructure NookOS deliberately avoids.
- **age/SOPS files in the repo**: good practice, orthogonal, and doesn't give
  the "paste your .env and go" flow this product wants.

## Acceptance criteria

- [ ] `SECRETS_KEY` + a full `pg_dump` cannot recover any plaintext (proven
      by a test that attempts exactly that).
- [ ] The control plane has no code path that produces secret plaintext.
- [ ] A node can write `.env` with the control plane never holding the DEK.
- [ ] Passphrase rotation re-wraps without re-encrypting.
- [ ] Losing every browser but keeping the recovery code restores access.
