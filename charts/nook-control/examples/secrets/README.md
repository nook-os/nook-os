# Populating the chart's Secret from a secret manager

**The contract:** the chart consumes exactly **one Kubernetes Secret**, by name
(`values.existingSecret`). How that Secret gets populated is entirely your
choice of tooling. **NookOS integrates with no secret manager directly** — there
is no Vault/GCP/AWS SDK in the control plane (by design); it only ever reads env
vars and files that come from a Kubernetes Secret. This is the pattern
Kubernetes expects: keep credentials in your backend, sync them into a Secret,
point the chart at that Secret.

## The keys the Secret must contain

The authoritative list lives in `crates/nook-control/src/config.rs`; these are
the keys wired from the Secret by `secretKeyRef` (see the
[chart README](../../README.md#secrets-by-reference)):

| Key | Required | What it is |
|---|---|---|
| `DATABASE_URL` | **yes** | external Postgres connection URL |
| `SESSION_SECRET` | **yes** | 32+ char session signing secret |
| `OIDC_CLIENT_SECRET` | if using OIDC | the confidential client secret |
| `NOOK_S3_ACCESS_KEY_ID` | if using S3 | object-store access key id |
| `NOOK_S3_SECRET_ACCESS_KEY` | if using S3 | object-store secret access key |
| `SECRETS_KEY` | recommended in prod | 64-hex key for NookOS's own at-rest secret encryption — see [`docs/secrets-encryption.md`](../../../../docs/secrets-encryption.md) |

> Two different "secrets" meet here, do not conflate them: the **deployment
> credentials** above (how the pod reaches Postgres/OIDC/S3) versus the
> **application sealed secrets** NookOS encrypts at rest inside the product,
> which are covered in [`docs/secrets-encryption.md`](../../../../docs/secrets-encryption.md).
> `SECRETS_KEY` is the bridge: a deployment credential that unlocks the latter.

## Worked examples (External Secrets Operator)

Each backend produces the **identical** Secret (`nook-control-secrets`), so the
`helm install … --set existingSecret=nook-control-secrets` command is unchanged
across all of them — only the source `SecretStore` differs:

- [`vault/`](vault/) — HashiCorp Vault (KV v2, Kubernetes auth)
- [`gcp/`](gcp/) — Google Secret Manager (Workload Identity)
- [`aws/`](aws/) — AWS Secrets Manager (IRSA)

All values in these manifests are placeholders (`ALL_CAPS`) — no real endpoints
or credentials. Installing or authenticating the External Secrets Operator, and
the provider auth (Vault k8s auth, Workload Identity, IRSA), are your cluster's
prerequisites, referenced here, not automated.

The chart does **not** depend on External Secrets — these are optional
companions. A hand-created Secret (`kubectl create secret generic …`) works
exactly as well.

## Not running External Secrets?

Two common alternatives produce the same Kubernetes Secret (pointers, not full
examples here):

- **[Secrets Store CSI driver](https://secrets-store-csi-driver.sigs.k8s.io/)**
  with a provider (Vault / GCP / AWS). Mounts secrets as a volume and, with
  `secretObjects`/`syncSecret` enabled, syncs them into a Kubernetes Secret you
  then pass as `existingSecret`.
- **[Vault Agent Injector](https://developer.hashicorp.com/vault/docs/platform/k8s/injector)**
  — annotate the pod and the sidecar renders secrets from Vault. To feed the
  chart's `existingSecret` contract, pair it with a small sync step (or prefer
  the CSI driver's `syncSecret`), since the chart wants a named Secret, not only
  a mounted file.
