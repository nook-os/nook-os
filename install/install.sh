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
case "$SERVER" in *@@*) SERVER="" ;; esac
case "$AGENT_URL" in *@@*) AGENT_URL="" ;; esac
case "$FINGERPRINT" in *@@*) FINGERPRINT="" ;; esac
case "$RELEASES" in *@@*) RELEASES="https://github.com/nook-os/nook-os/releases/latest/download" ;; esac

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
    --dry-run)     DRY=1; shift ;;
    --)            shift; PASSTHRU="$*"; break ;;
    -h|--help)
      cat <<'USAGE'
usage: install.sh [options]

  --node                 install the node agent (a machine that runs sessions)
  --control-plane        install a control plane (the thing machines connect to)
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
  A='\033[38;5;214m'; D='\033[2m'; R='\033[0m'; B='\033[1m'
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
    printf '  Choice [1]: '
  } > /dev/tty
  read -r reply < /dev/tty || die "input closed"
  case "${reply:-1}" in
    1) ROLE="node" ;;
    2) ROLE="control-plane" ;;
    *) die "expected 1 or 2" ;;
  esac
}

banner "NookOS" "$os/$arch"
[ -n "$ROLE" ] || ask_role

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
