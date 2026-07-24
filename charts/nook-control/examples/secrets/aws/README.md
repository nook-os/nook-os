# AWS Secrets Manager → Kubernetes Secret, via External Secrets

Sync credentials from **AWS Secrets Manager** into the Secret the chart
references. NookOS has no AWS SDK — the [External Secrets Operator][eso] reads
Secrets Manager and writes a plain Secret; the chart only reads that Secret.

## Prerequisites (yours to provide)

- The External Secrets Operator installed in the cluster.
- **IRSA** configured: the Kubernetes ServiceAccount named in `secret-store.yaml`
  is annotated with an IAM role allowing `secretsmanager:GetSecretValue` on the
  secret used. (NookOS does not automate this — NG-2.)
- The secret created in Secrets Manager as a JSON document (see the comment
  header in `external-secret.yaml`).

## Apply

1. Fill the `ALL_CAPS` placeholders in `secret-store.yaml` (`AWS_REGION`,
   `NOOK_NAMESPACE`) and `NOOK_NAMESPACE` in `external-secret.yaml`.
2. Create the secret (see `external-secret.yaml`'s header).
3. Apply:

   ```bash
   kubectl apply -f secret-store.yaml
   kubectl apply -f external-secret.yaml
   kubectl -n NOOK_NAMESPACE get secret nook-control-secrets
   ```

## Install the chart against it

Identical to every other backend — nothing here mentions AWS:

```bash
helm install nook charts/nook-control \
  --set existingSecret=nook-control-secrets \
  --set ingress.host=nook.example.com \
  --set config.publicBaseUrl=https://nook.example.com
```

See the [chart README](../../../README.md#secrets-by-reference) for the key list
and [`../README.md`](../README.md) for the other backends.

[eso]: https://external-secrets.io/
