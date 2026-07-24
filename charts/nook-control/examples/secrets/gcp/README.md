# Google Secret Manager → Kubernetes Secret, via External Secrets

Sync credentials from **Google Secret Manager** into the Secret the chart
references. NookOS has no GCP SDK — the [External Secrets Operator][eso] reads
Secret Manager and writes a plain Secret; the chart only reads that Secret.

## Prerequisites (yours to provide)

- The External Secrets Operator installed in the cluster.
- **Workload Identity** configured: the Kubernetes ServiceAccount named in
  `secret-store.yaml` is bound to a Google service account holding
  `roles/secretmanager.secretAccessor`. (NookOS does not automate this — NG-2.)
- The secrets created in Secret Manager, one per value (`nookos-database-url`,
  `nookos-session-secret`, and any optional ones).

## Apply

1. Fill the `ALL_CAPS` placeholders in `secret-store.yaml` (`GCP_PROJECT_ID`,
   `GCP_REGION`, `GKE_CLUSTER_NAME`, `NOOK_NAMESPACE`) and `NOOK_NAMESPACE` in
   `external-secret.yaml`.
2. Create the secrets (see the comment header in `external-secret.yaml`).
3. Apply:

   ```bash
   kubectl apply -f secret-store.yaml
   kubectl apply -f external-secret.yaml
   kubectl -n NOOK_NAMESPACE get secret nook-control-secrets
   ```

## Install the chart against it

Identical to every other backend — nothing here mentions GCP:

```bash
helm install nook charts/nook-control \
  --set existingSecret=nook-control-secrets \
  --set ingress.host=nook.example.com \
  --set config.publicBaseUrl=https://nook.example.com
```

See the [chart README](../../../README.md#secrets-by-reference) for the key list
and [`../README.md`](../README.md) for the other backends.

[eso]: https://external-secrets.io/
