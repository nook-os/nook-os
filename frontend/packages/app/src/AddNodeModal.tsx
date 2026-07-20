// Adding a machine to the fleet, in one command.
//
// The friction this removes is real: find the binary, get the right build for
// the architecture, put it on PATH, find the token, join, write a unit file.
// Six chances to end up on a different version than the server. So the modal
// mints the token, detects the platform, and hands over a single line that
// does all of it — with the download and the manual steps underneath for the
// machine that can't pipe curl to a shell.
import React, { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { Pill } from "@nookos/ui";
import { Check, Copy, Download, Terminal } from "lucide-react";

/** Every platform the server knows how to name, so the picker is stable
 *  whether or not a build for it exists yet. */
const PLATFORMS = [
  { os: "linux", arch: "x86_64", label: "Linux · x86_64" },
  { os: "linux", arch: "aarch64", label: "Linux · arm64" },
  { os: "darwin", arch: "aarch64", label: "macOS · Apple silicon" },
  { os: "darwin", arch: "x86_64", label: "macOS · Intel" },
];

type Artifact = {
  os: string;
  arch: string;
  label: string;
  filename: string;
  size: number;
  sha256: string;
  url: string;
};

/**
 * This browser's best guess at the machine it's running on.
 *
 * A guess, not a fact — a browser cannot see `uname`, and the machine being
 * added is often not this one anyway. So it only ever pre-selects, and the
 * picker stays visible.
 */
function detectPlatform(): { os: string; arch: string } {
  const ua = navigator.userAgent;
  const uaData = (navigator as unknown as { userAgentData?: { platform?: string } })
    .userAgentData;
  const platform = (uaData?.platform ?? navigator.platform ?? "").toLowerCase();
  const hay = `${platform} ${ua}`.toLowerCase();

  const os = hay.includes("mac") ? "darwin" : "linux";
  // Apple silicon is invisible to the browser — Safari and Chrome both report
  // Intel. For macOS the modern default is the better guess; for Linux, x86.
  const arch =
    os === "darwin"
      ? hay.includes("intel") && !hay.includes("arm")
        ? "aarch64"
        : "aarch64"
      : hay.includes("aarch64") || hay.includes("arm64")
        ? "aarch64"
        : "x86_64";
  return { os, arch };
}

function CopyLine({ value, label }: { value: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="addnode-cmd">
      {label && <div className="muted small">{label}</div>}
      <div className="addnode-cmd-row">
        <code className="mono bright" style={{ userSelect: "all" }}>
          {value}
        </code>
        <button
          className="btn small"
          title="copy"
          onClick={() => {
            navigator.clipboard.writeText(value);
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1200);
          }}
        >
          {copied ? <Check size={12} /> : <Copy size={12} />}
        </button>
      </div>
    </div>
  );
}

export function AddNodeModal({ onClose }: { onClose: () => void }) {
  const [token, setToken] = useState<string | null>(null);
  const [expiresAt, setExpiresAt] = useState<string | null>(null);
  const [picked, setPicked] = useState(detectPlatform);
  const [detected] = useState(detectPlatform);
  const [withSystemd, setWithSystemd] = useState(true);

  const { data: releases } = useQuery({
    queryKey: ["node", "releases"],
    queryFn: async () => (await api.GET("/api/v1/node/releases", {})).data,
  });

  // One fresh token per opening of this modal. Tokens are cheap, single-use in
  // spirit, and showing a stale one is how people end up pasting an expired
  // command into a machine they had to walk to.
  useEffect(() => {
    let live = true;
    (async () => {
      const { data } = await api.POST("/api/v1/nodes/join-tokens");
      if (live && data) {
        setToken(data.token);
        setExpiresAt(data.expires_at);
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  const artifacts = (releases?.artifacts ?? []) as Artifact[];
  // The browser's own origin is the one URL guaranteed to be reachable from
  // outside — it is, by definition, how someone got here. The server's idea of
  // its address is a fallback, because a proxy can rewrite Host to something
  // only the cluster can resolve.
  const server = window.location.origin || releases?.base_url || "";
  const downloadUrl = (a: Artifact) => `${server}/dist/${a.filename}`;
  const current =
    artifacts.find((a) => a.os === picked.os && a.arch === picked.arch) ?? null;

  const oneShot = token
    ? `curl -fLsS ${server}/install.sh | sh -s -- --token ${token}${
        withSystemd ? " --systemd" : ""
      }`
    : "…minting a join token…";
  const updateCmd = `curl -fLsS ${server}/install.sh | sh`;

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal"
        style={{ width: 720, maxHeight: "86vh", overflowY: "auto" }}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => e.key === "Escape" && onClose()}
      >
        <div className="modal-header">
          Add a node
          {releases?.version && (
            <span className="faint small" style={{ marginLeft: 8 }}>
              agent {releases.version}
            </span>
          )}
        </div>

        <div className="modal-body" style={{ display: "grid", gap: 14 }}>
          <section>
            <div className="addnode-step">
              <Terminal size={13} /> Run this on the new machine
            </div>
            <p className="muted small" style={{ margin: "2px 0 6px" }}>
              Downloads the agent this server is running, joins it to your
              fleet, and{withSystemd ? " installs a systemd service" : " leaves it to you to start"}.
              {expiresAt && (
                <> Token expires {new Date(expiresAt).toLocaleString()}.</>
              )}
            </p>
            <CopyLine value={oneShot} />
            <label className="small" style={{ display: "block", marginTop: 6 }}>
              <input
                type="checkbox"
                checked={withSystemd}
                onChange={(e) => setWithSystemd(e.target.checked)}
              />{" "}
              install a systemd service (Linux; asks for sudo)
            </label>
          </section>

          <section>
            <div className="addnode-step">
              <Download size={13} /> Or download the binary
            </div>
            <div className="addnode-platforms">
              {PLATFORMS.map((a) => {
                const active = a.os === picked.os && a.arch === picked.arch;
                const available = artifacts.some(
                  (x) => x.os === a.os && x.arch === a.arch,
                );
                // Until the list loads, "not built" would be a lie about every
                // platform — say nothing rather than something wrong.
                const known = !!releases;
                return (
                  <button
                    key={`${a.os}-${a.arch}`}
                    className={`addnode-platform${active ? " active" : ""}`}
                    onClick={() => setPicked({ os: a.os, arch: a.arch })}
                  >
                    {a.label}
                    {a.os === detected.os && a.arch === detected.arch && (
                      <Pill tone="dim">detected</Pill>
                    )}
                    {known && !available && <Pill tone="warn">not built</Pill>}
                  </button>
                );
              })}
            </div>

            {current ? (
              <div style={{ marginTop: 8 }}>
                <a className="btn small primary" href={downloadUrl(current)} download>
                  <Download size={12} /> {current.filename}
                </a>{" "}
                <span className="faint small">
                  {(current.size / 1_000_000).toFixed(1)} MB · sha256{" "}
                  {current.sha256.slice(0, 12)}…
                </span>
                <CopyLine
                  label="Or fetch it directly:"
                  value={`curl -fLsS ${downloadUrl(current)} -o nook && chmod +x nook`}
                />
              </div>
            ) : (
              <p className="muted small" style={{ marginTop: 8 }}>
                This server has no build for that platform. It ships the
                binaries it was built with — add a cross-built artifact named{" "}
                <code className="mono">nook-{picked.os}-{picked.arch}</code> to
                the control plane's dist directory, or build from source:{" "}
                <code className="mono">cargo build --release -p nook-node</code>.
              </p>
            )}
          </section>

          <section>
            <div className="addnode-step">Already joined? Keep it in step</div>
            <p className="muted small" style={{ margin: "2px 0 6px" }}>
              Same script with no token: updates the binary in place and leaves
              the node's config alone. <code className="mono">nook update</code>{" "}
              on the machine does the same thing.
            </p>
            <CopyLine value={updateCmd} />
          </section>

          {token && (
            <section>
              <div className="addnode-step">Manual join</div>
              <CopyLine
                value={`nook join --server ${server} --token ${token}`}
              />
            </section>
          )}
        </div>

        <div className="modal-footer">
          <button className="btn" onClick={onClose}>
            done
          </button>
        </div>
      </div>
    </div>
  );
}
