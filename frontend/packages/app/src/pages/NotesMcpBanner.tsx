// A dismissible banner on the Notes page (MAIN-180): Notes can be driven from
// ChatGPT or Claude through this instance's `/mcp` endpoint with a personal
// access token, but nothing in the UI said so. This points users at the
// endpoint URL and where to mint a token. It reuses the existing `/mcp` endpoint
// and PATs (NG-1/NG-2) and is Notes-only (NG-3).
import React, { useState } from "react";
import { Link } from "react-router-dom";
import { Plug, X } from "lucide-react";

/** Per-browser dismissal, so it stays hidden on return (AC-3). localStorage is
 *  per-origin, i.e. per-user on a normal single-account browser profile. */
const DISMISS_KEY = "nook.notesMcpBannerDismissed";

/** The absolute MCP endpoint for THIS instance — `<origin>/mcp` (AC-2), so the
 *  URL a user copies works verbatim wherever the app is served. */
function mcpUrl(): string {
  return `${window.location.origin}/mcp`;
}

export function NotesMcpBanner() {
  const [dismissed, setDismissed] = useState(
    () => localStorage.getItem(DISMISS_KEY) === "1",
  );
  const [copied, setCopied] = useState(false);
  if (dismissed) return null;

  const url = mcpUrl();

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard can be refused (insecure origin / denied permission) — the URL
      // stays on screen to copy by hand, so nothing is lost.
    }
  };

  const dismiss = () => {
    localStorage.setItem(DISMISS_KEY, "1");
    setDismissed(true);
  };

  return (
    <div className="notebook-mcp-banner" role="note">
      <Plug size={13} />
      <div className="notebook-mcp-banner-body">
        <div className="notebook-mcp-url">
          <code className="mono">{url}</code>
          <button type="button" className="btn small" onClick={copy}>
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
        <p>
          Add this as an MCP connector in ChatGPT or Claude, using an access
          token as the bearer.
        </p>
        <p>
          <Link to="/settings">Settings → Access tokens</Link> to mint one.
        </p>
      </div>
      <button
        type="button"
        className="notebook-mcp-dismiss"
        title="dismiss"
        aria-label="dismiss MCP banner"
        onClick={dismiss}
      >
        <X size={12} />
      </button>
    </div>
  );
}
