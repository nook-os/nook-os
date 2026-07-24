# Vault → Kubernetes Secret, via External Secrets

Sync credentials from **HashiCorp Vault** into the Kubernetes Secret the chart
references. NookOS never talks to Vault — the [External Secrets Operator][eso]
reads Vault and writes a plain Secret; the chart only ever reads that Secret.

## Prerequisites (yours to provide)

- The External Secrets Operator installed in the cluster.
- A Vault reachable from the cluster, with a **KV v2** secret at
  `secret/nookos/control-plane` holding the fields the `ExternalSecret` maps
  (`database_url`, `session_secret`, and any optional ones you use).
- Vault **Kubernetes auth** configured with a role bound to the ServiceAccount
  the operator runs as. NookOS does not set this up (it is your cluster's
  prerequisite).

## Apply

1. Fill in every `ALL_CAPS` placeholder in `secret-store.yaml`
   (`NOOK_NAMESPACE`, `NOOK_VAULT_ROLE`, the Vault address) and
   `external-secret.yaml` (`NOOK_NAMESPACE`).
2. Put the values in Vault, e.g.:

   ```bash
   vault kv put secret/nookos/control-plane \
     database_url='postgres://user:pass@db.example:5432/nook' \
     session_secret="$(openssl rand -hex 32)"
   ```

3. Apply the store, then the ExternalSecret:

   ```bash
   kubectl apply -f secret-store.yaml
   kubectl apply -f external-secret.yaml
   ```

4. ESO creates the Secret. Confirm it has the expected keys:

   ```bash
   kubectl -n NOOK_NAMESPACE get secret nook-control-secrets \
     -o jsonpath='{.data}' | tr ',' '\n'
   ```

## Install the chart against it

The Secret name is all the chart needs — nothing here mentions Vault:

```bash
helm install nook charts/nook-control \
  --set existingSecret=nook-control-secrets \
  --set ingress.host=nook.example.com \
  --set config.publicBaseUrl=https://nook.example.com
```

See the [chart README](../../../README.md#secrets-by-reference) for the full key
list, and [`../README.md`](../README.md) for the GCP/AWS equivalents and the
non-ESO alternatives.

[eso]: https://external-secrets.io/
