#!/usr/bin/env bash
# Turn on the mTLS agent port on a production control plane.
#
# The agent listener cannot sit behind the reverse proxy the API uses. TLS has
# to terminate in the control-plane process, because only it knows which
# tenant's CA a given client certificate should be judged against — a proxy
# that terminated TLS would hold the certificate and hand us plaintext. So the
# port is published directly and nodes pin its certificate by fingerprint,
# which is why a self-signed certificate is not a compromise here: the pin is
# strictly stronger than "some public CA vouched for this name".
#
# Idempotent. Run it from the deployment directory (the one with
# docker-compose.prod.yml). Restarts nothing — it prints what to run.
set -euo pipefail

COMPOSE=${COMPOSE:-docker-compose.prod.yml}
CERT_DIR=${CERT_DIR:-agent-certs}
HOSTNAME_SAN=${HOSTNAME_SAN:-$(grep -oP '(?<=^PUBLIC_BASE_URL=https://)[^/]+' .env 2>/dev/null || echo localhost)}

[[ -f $COMPOSE ]] || { echo "no $COMPOSE here — run this from the deployment directory" >&2; exit 1; }

# ---------------------------------------------------------------- certificate
mkdir -p "$CERT_DIR"
if [[ ! -f $CERT_DIR/agent.crt ]]; then
  echo "▸ generating an agent certificate for $HOSTNAME_SAN"
  # Ten years: rotating this means re-pinning every node, and the certificate
  # is not what authenticates anyone — the client certificates are.
  openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$CERT_DIR/agent.key" -out "$CERT_DIR/agent.crt" \
    -subj "/CN=$HOSTNAME_SAN" \
    -addext "subjectAltName=DNS:$HOSTNAME_SAN" >/dev/null 2>&1
  chmod 600 "$CERT_DIR/agent.key"
else
  echo "▸ reusing the existing certificate in $CERT_DIR"
fi

FINGERPRINT=$(openssl x509 -in "$CERT_DIR/agent.crt" -outform der | sha256sum | cut -d' ' -f1)

# ------------------------------------------------------------------- compose
python3 - "$COMPOSE" "$CERT_DIR" <<'PY'
import re, sys
path, cert_dir = sys.argv[1], sys.argv[2]
s = open(path).read()
if "NOOK_AGENT_TLS_CERT" in s:
    print("▸ compose file already wired for the agent port")
    raise SystemExit(0)

block = f'''    ports:
      - "8081:8081"
    environment:
      NOOK_AGENT_BIND: 0.0.0.0:8081
      NOOK_AGENT_TLS_CERT: /etc/nook/agent.crt
      NOOK_AGENT_TLS_KEY: /etc/nook/agent.key
    volumes:
      - ./{cert_dir}:/etc/nook:ro
'''
# Anchor on the control-plane service's `expose:` line, which is unique.
new, n = re.subn(r'(\n  control-plane:\n(?:.*\n)*?    expose: \["8080"\]\n)',
                 lambda m: m.group(1) + block, s, count=1)
if n != 1:
    print("!! could not find the control-plane service — patch it by hand:", file=sys.stderr)
    print(block, file=sys.stderr)
    raise SystemExit(1)
open(path + ".bak-mtls", "w").write(s)
open(path, "w").write(new)
print(f"▸ patched {path} (previous version saved as {path}.bak-mtls)")
PY

cat <<EOF

  Agent certificate fingerprint:

    $FINGERPRINT

  Apply it:

    docker compose -f $COMPOSE up -d control-plane

  Then, on each node (needs sudo if the agent runs as a service):

    nook enroll \\
      --server https://$HOSTNAME_SAN:8081 \\
      --token <join token from the UI> \\
      --server-fingerprint $FINGERPRINT
    sudo systemctl restart nook-node

  The node keeps its old bearer token until it reconnects, so a node that
  fails to enrol stays on the connection it already has rather than dropping
  off the fleet.
EOF
