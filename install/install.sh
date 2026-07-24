#!/bin/sh
# NookOS installer.
#
#   curl -fsSL https://nookos.dev/install.sh | sh                    # asks what to install
#   curl -fsSL https://<your-nook>/install.sh | sh -s -- --token …   # a node, on this machine
#
# Deliberately small. Its whole job is to put a verified binary on disk and get
# out of the way; every question a person is actually asked lives in the binary,
# where it can be unit-tested and behaves the same on every platform.
#
# Reading this before running it is the correct instinct:
#   curl -fsSL https://nookos.dev/install.sh -o install.sh && less install.sh
set -eu

# Everything lives in main(), called on the last line, so the shell parses the
# whole file before running any of it.
#
# Without this, `curl … | sh` is a race: the shell reads a chunk, starts
# executing, and when we `exec` into the binary the rest of the script is still
# in flight. curl finds the pipe closed and reports "(23) Failure writing output
# to destination" — its error, about our success, and the reader concludes the
# installer is broken.
#
# Left unindented on purpose: the heredoc terminators below have to stay at
# column zero, and a body that is half-indented is worse than one that is not.
main() {

# @@SERVER@@ is substituted when the control plane serves this file, so a node
# installer knows where to phone home. Served from nookos.dev the placeholder
# survives verbatim, and the wizard asks instead.
SERVER="@@SERVER@@"
AGENT_URL="@@AGENT_URL@@"
FINGERPRINT="@@FINGERPRINT@@"
RELEASES="@@RELEASES@@"
# The chart version the k8s hand-off pins, substituted from the serving control
# plane's own version (dist.rs already injects @@VERSION@@). Served from the
# generic domain the placeholder survives → empty → the printed command omits
# --version and helm pulls the latest published chart.
CHART_VERSION="@@VERSION@@"
case "$SERVER" in *@@*) SERVER="" ;; esac
case "$AGENT_URL" in *@@*) AGENT_URL="" ;; esac
case "$FINGERPRINT" in *@@*) FINGERPRINT="" ;; esac
case "$RELEASES" in *@@*) RELEASES="https://github.com/nook-os/nook-os/releases/latest/download" ;; esac
case "$CHART_VERSION" in *@@*) CHART_VERSION="" ;; esac

ROLE=""
TOKEN=""
NAME=""
PREFIX="${NOOK_PREFIX:-$HOME/.local/bin}"
DRY=0
PASSTHRU=""

while [ $# -gt 0 ]; do
  case "$1" in
    --server)      SERVER="${2:-}"; shift 2 ;;
    --token)       TOKEN="${2:-}"; ROLE="node"; shift 2 ;;
    --name)        NAME="${2:-}"; shift 2 ;;
    --prefix)      PREFIX="${2:-}"; shift 2 ;;
    --fingerprint) FINGERPRINT="${2:-}"; shift 2 ;;
    --node)        ROLE="node"; shift ;;
    --control-plane|--server-role) ROLE="control-plane"; shift ;;
    --k8s|--kubernetes) ROLE="k8s"; shift ;;
    --dry-run)     DRY=1; shift ;;
    --)            shift; PASSTHRU="$*"; break ;;
    -h|--help)
      cat <<'USAGE'
usage: install.sh [options]

  --node                 install the node agent (a machine that runs sessions)
  --control-plane        install a control plane (the thing machines connect to)
  --k8s                  Kubernetes: print the helm command + write nook-values.yaml
  --token TOKEN          join token; implies --node
  --name NAME            node name (default: this machine's hostname)
  --server URL           control plane URL
  --fingerprint SHA256   certificate to pin, from the join token
  --prefix DIR           where to put the binary (default ~/.local/bin)
  --dry-run              show what would happen, change nothing

With no role given, it asks.
USAGE
      exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------- presentation
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  # A REAL escape byte, not the four characters \ 0 3 3. `printf '%s'` does not
  # interpret backslash escapes in its ARGUMENTS — only in the format string —
  # so a variable holding "\033[..." prints literally, which is what shipped
  # and what you saw. Building the byte once here keeps every call site a
  # plain, shellcheck-clean '%s'.
  esc=$(printf '\033')
  A="${esc}[38;5;214m"; D="${esc}[2m"; R="${esc}[0m"; B="${esc}[1m"
else
  A=''; D=''; R=''; B=''
fi
# Colours travel as arguments, never inside the format string: a `%` arriving
# through a variable would be read as a conversion and mangle the output.
say()  { printf '%s▸%s %s\n' "$A" "$R" "$*"; }
ok()   { printf '%s✓%s %s\n' "$A" "$R" "$*"; }
die()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }
banner() {
  printf '\n%s%s  ┌┐╔─┐┌─┐┬┌─%s\n' "$A" "$B" "$R"
  printf '%s%s  ││║ ││ ││├┴┐%s   %s\n' "$A" "$B" "$R" "$1"
  printf '%s%s  ┘└╚─┘└─┘┴┴ ┴%s   %s%s%s\n\n' "$A" "$B" "$R" "$D" "$2" "$R"
}

# ---------------------------------------------------------------- prerequisites
command -v curl >/dev/null 2>&1 || die "curl is required"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$os" in
  linux|darwin) ;;
  *) die "unsupported OS '$os'. Build from source: cargo build --release -p nook-node" ;;
esac
case "$arch" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture '$arch'" ;;
esac
artifact="nook-$os-$arch"

# ---------------------------------------------------------------- what to install
#
# Under `curl … | sh` stdin is this script, so a question read from stdin eats
# the installer instead of waiting for an answer. Read the terminal directly.
ask_role() {
  # `[ -r /dev/tty ]` is not the test. In a container the device node exists
  # and looks readable, but opening it fails with ENXIO because the process has
  # no controlling terminal — so the check passes and the redirect then dies
  # with a raw shell error. Actually try to open it.
  # Run the probe in a SUBSHELL. A redirection that fails on a compound command
  # is fatal in dash — it kills the script with status 2 before `die` can say
  # anything, so the check has to happen somewhere its death is survivable.
  if ! (exec < /dev/tty) 2>/dev/null; then
    die "no terminal to ask on — pass --node or --control-plane"
  fi
  {
    printf '  What is this machine?\n'
    printf '    [1] A %snode%s — runs your code and agent sessions\n' "$B" "$R"
    printf '    [2] A %scontrol plane%s — the thing nodes connect to\n' "$B" "$R"
    printf '    [3] %sKubernetes%s — hand off to Helm (prints the command)\n' "$B" "$R"
    printf '  Choice [1]: '
  } > /dev/tty
  read -r reply < /dev/tty || die "input closed"
  case "${reply:-1}" in
    1) ROLE="node" ;;
    2) ROLE="control-plane" ;;
    3) ROLE="k8s" ;;
    *) die "expected 1, 2 or 3" ;;
  esac
}

# Kubernetes install — a hand-off, not an install. install.sh stays tiny and
# dependency-free: it prints the exact `helm install` command and writes a
# starter values file, and does NOT drive helm/kubectl (NG-1). Needs neither
# tool present (AC-4): it only prints and writes.
k8s_handoff() {
  chart="oci://ghcr.io/nook-os/charts/nook-control"
  values="nook-values.yaml"

  # A curated subset of charts/nook-control/values.yaml — the keys an operator
  # must set. No secret material (NG-2): the Secret is created separately and
  # only NAMED here.
  cat > "$values" <<'YAML'
# NookOS control plane — starter values for `helm install` (install.sh --k8s).
# Fill in the placeholders below. Nothing here is a secret.
#
# The chart consumes ONE Kubernetes Secret, by name. Create it first — populate
# it from your backend with the External Secrets examples under
# charts/nook-control/examples/secrets/ (Vault / GCP / AWS), or by hand:
#
#   kubectl create secret generic nook-control-secrets \
#     --from-literal=DATABASE_URL='postgres://user:pass@db.example.com:5432/nook' \
#     --from-literal=SESSION_SECRET="$(openssl rand -hex 32)"

# Name of that Secret (REQUIRED). The chart never stores secret material itself.
existingSecret: nook-control-secrets

config:
  # The external URL people reach the UI at (usually https://<ingress.host>).
  publicBaseUrl: https://nook.example.com
  # Allowed browser origin — usually the same as publicBaseUrl.
  webOrigin: https://nook.example.com

ingress:
  # The hostname that routes to NookOS (REQUIRED for a real install).
  host: nook.example.com

# Agent mTLS listener (:8081) — how EXTERNAL nodes join a cluster-hosted control
# plane. Off by default. To enable, supply a TLS Secret holding the listener
# cert and the externally reachable address; see the chart README, "Agent mTLS
# listener", for generating the cert and the passthrough requirement.
agent:
  enabled: false
  # publicUrl: agent.nook.example.com:8081
  # tlsSecret: nook-agent-tls
YAML

  ok "Wrote starter values: ./$values"
  say "Kubernetes install — nothing was downloaded; NookOS runs from the chart."
  echo
  say "1. Create the Secret it references (see the top of $values)."
  say "2. Edit $values (host, URLs, agent), then install:"
  echo
  if [ -n "$CHART_VERSION" ]; then
    printf '     helm install nook %s \\\n' "$chart"
    printf '       --version %s \\\n' "$CHART_VERSION"
    printf '       -f %s\n' "$values"
  else
    printf '     helm install nook %s \\\n' "$chart"
    printf '       -f %s\n' "$values"
    say "   (no pinned version — helm pulls the latest published chart; add"
    say "    --version X.Y.Z to pin it.)"
  fi
  echo
  say "Secrets from Vault/GCP/AWS: charts/nook-control/examples/secrets/"

  # AC-4: this path needs neither tool to be present. If they are missing, say
  # what to install to RUN the printed command — but still exit success.
  command -v helm >/dev/null 2>&1 \
    || say "Helm 3 is not installed here — get it to run the command above: https://helm.sh/docs/intro/install/"
  command -v kubectl >/dev/null 2>&1 \
    || say "kubectl is not installed here — you'll need it pointed at your cluster."
}

banner "nook@os" "$os/$arch"
[ -n "$ROLE" ] || ask_role

# Kubernetes hands off to Helm before any binary logic — it downloads nothing
# and needs no binary for this machine (AC-1/AC-4). Additive: the node and
# control-plane paths below are untouched (AC-5).
if [ "$ROLE" = "k8s" ]; then
  k8s_handoff
  exit 0
fi

if [ "$DRY" = "1" ]; then
  say "Would install $artifact from $RELEASES"
  say "Would place it at $PREFIX/nook"
  say "Would then run: nook $( [ "$ROLE" = node ] && echo setup || echo 'server init' )"
  exit 0
fi

# ---------------------------------------------------------------- download
tmp=$(mktemp) || die "cannot create a temporary file"
sum=$(mktemp) || die "cannot create a temporary file"
trap 'rm -f "$tmp" "$sum"' EXIT INT TERM

say "Downloading $artifact"
curl -fLsS "$RELEASES/$artifact" -o "$tmp" \
  || die "no build published for $os/$arch"

# Verify against the checksum published beside it. Not a substitute for
# reviewing the script, but it does mean a corrupted or truncated download
# fails loudly instead of installing something that half works.
if curl -fLsS "$RELEASES/$artifact.sha256" -o "$sum" 2>/dev/null; then
  expected=$(cut -d' ' -f1 < "$sum")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp" | cut -d' ' -f1)
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
  else
    actual=""
  fi
  if [ -n "$actual" ]; then
    [ "$actual" = "$expected" ] || die "checksum mismatch — expected $expected, got $actual"
    ok "Checksum verified"
  else
    say "No sha256 tool found; skipping verification"
  fi
else
  say "No published checksum; skipping verification"
fi

chmod +x "$tmp"
mkdir -p "$PREFIX"
# Rename rather than overwrite: writing over a running binary fails with
# ETXTBSY, which is precisely the case when updating a live node.
mv -f "$tmp" "$PREFIX/nook"
trap - EXIT INT TERM
rm -f "$sum"
ok "Installed $PREFIX/nook ($("$PREFIX/nook" --version 2>/dev/null || echo unknown))"

case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) say "Not on your PATH yet:  export PATH=\"$PREFIX:\$PATH\"" ;;
esac

# ---------------------------------------------------------------- hand off
#
# Everything interactive happens in the binary from here.
if [ "$ROLE" = "control-plane" ]; then
  # shellcheck disable=SC2086
  exec "$PREFIX/nook" server init $PASSTHRU
fi

set -- setup
[ -n "$SERVER" ]      && set -- "$@" --server "$SERVER"
[ -n "$AGENT_URL" ]   && set -- "$@" --agent-url "$AGENT_URL"
[ -n "$TOKEN" ]       && set -- "$@" --token "$TOKEN"
[ -n "$NAME" ]        && set -- "$@" --name "$NAME"
[ -n "$FINGERPRINT" ] && set -- "$@" --fingerprint "$FINGERPRINT"
# shellcheck disable=SC2086
exec "$PREFIX/nook" "$@" $PASSTHRU
}

main "$@"
