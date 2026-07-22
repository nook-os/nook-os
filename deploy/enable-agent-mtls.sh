#!/usr/bin/env bash
# Turn on the mTLS agent port on a production control plane.
#
# The agent listener cannot sit behind a proxy the way the API does. TLS has to
# terminate in the control-plane process, because only it knows which tenant's
# CA a given client certificate should be judged against — a proxy that
# terminated TLS would hold the certificate and hand us plaintext.
#
# So this uses Traefik in TCP **passthrough** mode: it routes on the SNI name
# and never opens the stream. Node connections ride the 443 that is already
# open and arrive at port 8081 untouched, which means no new firewall rule and
# no port-forward — the usual reason a second listener never makes it past the
# edge of someone's network.
#
# The certificate is self-signed on purpose. Nodes pin its fingerprint, which
# is strictly stronger than trusting any public CA that could be persuaded to
# issue for the hostname.
#
# Idempotent. Run it from the deployment directory (the one holding
# docker-compose.prod.yml). It restarts nothing — it prints what to run.
set -euo pipefail

COMPOSE=${COMPOSE:-docker-compose.prod.yml}
CERT_DIR=${CERT_DIR:-agent-certs}
BASE_HOST=${BASE_HOST:-$(grep -oP '(?<=^PUBLIC_BASE_URL=https://)[^/]+' .env 2>/dev/null || echo localhost)}
# A name of its own, so SNI can tell agent traffic from the API on the same
# port. It must resolve to the same address as the API.
AGENT_HOST=${AGENT_HOST:-agent.$BASE_HOST}
ENTRYPOINT=${ENTRYPOINT:-websecure}

[[ -f $COMPOSE ]] || { echo "no $COMPOSE here — run this from the deployment directory" >&2; exit 1; }

# ---------------------------------------------------------------- certificate
mkdir -p "$CERT_DIR"
if [[ ! -f $CERT_DIR/agent.crt ]]; then
  echo "▸ generating an agent certificate for $AGENT_HOST"
  # Ten years: re-pinning every node is the cost of rotating this, and it is
  # not what authenticates anyone — the client certificates are.
  openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$CERT_DIR/agent.key" -out "$CERT_DIR/agent.crt" \
    -subj "/CN=$AGENT_HOST" \
    -addext "subjectAltName=DNS:$AGENT_HOST,DNS:$BASE_HOST" >/dev/null 2>&1
  chmod 600 "$CERT_DIR/agent.key"
else
  echo "▸ reusing the existing certificate in $CERT_DIR"
fi

FINGERPRINT=$(openssl x509 -in "$CERT_DIR/agent.crt" -outform der | sha256sum | cut -d' ' -f1)

# ------------------------------------------------------------------- compose
python3 - "$COMPOSE" "$CERT_DIR" "$AGENT_HOST" "$ENTRYPOINT" <<'PY'
import re, sys
path, cert_dir, agent_host, entrypoint = sys.argv[1:5]
s = open(path).read()
if "NOOK_AGENT_TLS_CERT" in s:
    print("▸ compose file already wired for the agent port")
    raise SystemExit(0)

block = f'''    environment:
      NOOK_AGENT_BIND: 0.0.0.0:8081
      NOOK_AGENT_TLS_CERT: /etc/nook/agent.crt
      NOOK_AGENT_TLS_KEY: /etc/nook/agent.key
    volumes:
      - ./{cert_dir}:/etc/nook:ro
'''
labels = f'''      - "traefik.tcp.routers.nook-agent.rule=HostSNI(`{agent_host}`)"
      - "traefik.tcp.routers.nook-agent.entrypoints={entrypoint}"
      # Passthrough, not termination: the control plane must see the client
      # certificate itself, so the proxy may route this stream but never open it.
      - "traefik.tcp.routers.nook-agent.tls.passthrough=true"
      - "traefik.tcp.services.nook-agent.loadbalancer.server.port=8081"
'''

# Anchor on the control-plane service's `expose:` line, which is unique.
s2, n = re.subn(r'(\n  control-plane:\n(?:.*\n)*?    expose: \["8080"\]\n)',
                lambda m: m.group(1) + block, s, count=1)
if n != 1:
    print("!! could not find the control-plane service — add by hand:", file=sys.stderr)
    print(block + labels, file=sys.stderr)
    raise SystemExit(1)

# And append the TCP router to that service's existing label list.
s3, n = re.subn(r'(      - "traefik\.http\.services\.nook-api\.loadbalancer\.server\.port=8080"\n)',
                lambda m: m.group(1) + labels, s2, count=1)
if n != 1:
    print("!! could not find the nook-api labels — add by hand:", file=sys.stderr)
    print(labels, file=sys.stderr)
    raise SystemExit(1)

open(path + ".bak-mtls", "w").write(s)
open(path, "w").write(s3)
print(f"▸ patched {path} (previous version saved as {path}.bak-mtls)")
PY

cat <<EOF

  Agent certificate fingerprint:

    $FINGERPRINT

  1. Point $AGENT_HOST at the same address as $BASE_HOST.
     If it is behind a CDN, this record must bypass it — a proxy that
     terminates TLS breaks both the pin and the client certificate.

  2. Apply:

       docker compose -f $COMPOSE up -d control-plane

  3. On each node (sudo if the agent runs as a service):

       nook enroll \\
         --server https://$AGENT_HOST \\
         --token <join token from the UI> \\
         --server-fingerprint $FINGERPRINT
       sudo systemctl restart nook-node

  A node keeps its existing connection until it reconnects, so one that fails
  to enrol stays on the fleet rather than dropping off it.
EOF
