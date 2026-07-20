// xterm.js wrapper, transport-agnostic: the parent supplies an `attach`
// callback so this component works over any byte stream.
import React, { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import "@xterm/xterm/css/xterm.css";
import { terminalTheme, useTheme } from "./theme";

// Bundled JuliaMono is primary for the terminal: it's the one monospace face
// that covers every glyph a TUI throws at it — box-drawing, blocks, shapes,
// arrows, braille, powerline, and the Misc-Technical / media symbols Claude
// Code uses (⎿ ⏵ ⏺) that JetBrains Mono / Cascadia / DejaVu all lack. Rendering
// everything from one font also keeps a single, consistent cell width. The rest
// are only reached for anything even JuliaMono somehow misses.
const TERM_FONT =
  "'JuliaMono', 'JetBrains Mono Variable', 'JetBrains Mono', 'Cascadia Mono', Consolas, Menlo, 'DejaVu Sans Mono', ui-monospace, monospace";

export interface TerminalHandlers {
  onOutput(bytes: Uint8Array): void;
  onStatus?(status: string): void;
  /** The session's agreed grid: the PTY follows the driver (last typer). */
  onSize?(cols: number, rows: number): void;
  onClose?(): void;
}

export interface TerminalTransport {
  sendInput(bytes: Uint8Array): void;
  resize(cols: number, rows: number): void;
  close(): void;
}

export function TerminalView({
  attach,
  onStatus,
}: {
  /** Open the transport; called once on mount. */
  attach(handlers: TerminalHandlers): TerminalTransport;
  onStatus?(status: string): void;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const { tokens } = useTheme();

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const term = new Terminal({
      fontFamily: tokens.fonts?.mono ?? TERM_FONT,
      fontSize: 13,
      // Slightly looser rows read like a native terminal and stop box-drawing
      // characters from visually colliding.
      lineHeight: 1.05,
      theme: terminalTheme(tokens),
      cursorBlink: true,
      cursorStyle: "block",
      // tmux owns ALL history (mouse wheel = copy-mode scroll). A local
      // scrollback would only collect stale tmux frames — every resize or
      // reattach redraw pushed old rows up where they reflowed into garbage
      // (doubled status bars, text fragments). Pure viewport instead.
      scrollback: 0,
      convertEol: false,
      // Draw box-drawing and block glyphs ourselves instead of trusting the
      // font — pixel-perfect corners/lines regardless of which font resolves.
      customGlyphs: true,
      // Correct cell widths for wide/ambiguous glyphs (the unicode11 addon
      // needs proposed APIs to register its width provider).
      allowProposedApi: true,
    });
    let disposed = false;

    const fit = new FitAddon();
    term.loadAddon(fit);
    // Accurate character widths → box-drawing art and CJK line up.
    term.loadAddon(new Unicode11Addon());
    // OSC 52 clipboard: tmux (`set-clipboard on`) emits copy-mode selections
    // as OSC 52 — this routes them into the system clipboard, so a mouse
    // drag-select inside tmux IS a copy.
    term.loadAddon(new ClipboardAddon());
    term.open(host);
    // Switch width tables only AFTER open() — before the render service exists
    // the version setter's re-wrap throws reading renderer `dimensions`.
    term.unicode.activeVersion = "11";

    // ── Shared-session sizing ──────────────────────────────────────────────
    // The PTY follows the DRIVER: whoever typed most recently. The driver fits
    // LOCALLY and instantly (native-feeling resize) and tells the server; the
    // server's Size echo is a no-op confirmation. Spectators never resize the
    // PTY — they render the driver's grid, scaling their font down to fit, so
    // a tiny read-only window can't shrink the session for the person working.
    const BASE_FONT = 13;
    const MIN_FONT = 7;
    let grid: { cols: number; rows: number } | null = null;
    let lastVote: { cols: number; rows: number } | null = null;
    // First viewer defaults to driver server-side; corrected by the first Size
    // that doesn't match our own vote.
    let isDriver = true;

    const propose = () => {
      try {
        return fit.proposeDimensions();
      } catch {
        return undefined; // renderer not ready / host mid-layout
      }
    };

    /** Spectator path: render the agreed grid, scaling the font to fit this
     *  panel. Font metrics settle asynchronously, so this takes one
     *  proportional step per call and re-checks next frame until stable. */
    let fitPasses = 0;
    const applyGrid = () => {
      if (disposed || !grid || !host.isConnected || host.clientHeight === 0) return;
      const d = propose();
      if (d && Number.isFinite(d.cols) && Number.isFinite(d.rows)) {
        const cur = term.options.fontSize ?? BASE_FONT;
        // How much the font could grow/shrink for the grid to exactly fit.
        const scale = Math.min(d.cols / grid.cols, d.rows / grid.rows);
        const next = Math.max(MIN_FONT, Math.min(BASE_FONT, Math.floor(cur * scale)));
        if (next !== cur && fitPasses < 4) {
          fitPasses += 1;
          term.options.fontSize = next;
          requestAnimationFrame(applyGrid); // re-measure after metrics settle
        } else {
          fitPasses = 0;
        }
      }
      if (term.cols !== grid.cols || term.rows !== grid.rows) {
        try {
          term.resize(grid.cols, grid.rows);
        } catch {
          // renderer not ready
        }
      }
    };

    let voteTimer: ReturnType<typeof setTimeout> | undefined;
    /** The vote most recently SENT to the server (echoes match against this). */
    let lastSent: { cols: number; rows: number } | null = null;
    const sendVote = () => {
      if (disposed || !lastVote) return;
      lastSent = lastVote;
      transport.resize(lastVote.cols, lastVote.rows);
    };

    /** Measure this panel's natural grid, apply it locally when we're the
     *  driver, and (debounced) tell the server. */
    const vote = () => {
      if (disposed || !host.isConnected || host.clientHeight === 0) return;
      const d = propose();
      if (!d || !Number.isFinite(d.cols) || !Number.isFinite(d.rows)) return;
      // proposeDimensions() reflects the CURRENT font; normalize to base so a
      // font-shrunk spectator doesn't vote an inflated size.
      const f = (term.options.fontSize ?? BASE_FONT) / BASE_FONT;
      const cols = Math.max(20, Math.round(d.cols * f));
      const rows = Math.max(5, Math.round(d.rows * f));
      lastVote = { cols, rows };
      if (isDriver) {
        grid = { cols, rows };
        if (term.options.fontSize !== BASE_FONT) term.options.fontSize = BASE_FONT;
        try {
          term.resize(cols, rows);
        } catch {
          // renderer not ready
        }
      }
      clearTimeout(voteTimer);
      voteTimer = setTimeout(sendVote, 120);
    };

    /** Re-fit after anything that changes available space or metrics. */
    const refit = () => {
      if (!isDriver) applyGrid();
      vote();
    };

    // Cell metrics are measured at open(); if the bundled font is still loading
    // they're wrong (misaligned boxes, dropped glyphs). Re-measure once it's
    // actually available.
    const remeasureOnFont = () => {
      const fonts = (document as Document & { fonts?: FontFaceSet }).fonts;
      if (!fonts?.load) return;
      Promise.all([
        fonts.load('13px "JuliaMono"'),
        fonts.load('bold 13px "JuliaMono"'),
      ])
        .then(() => {
          if (disposed) return;
          try {
            term.clearTextureAtlas();
          } catch {
            // no-op for the DOM renderer / disposed term
          }
          refit();
        })
        .catch(() => {});
    };
    remeasureOnFont();

    // ── Native copy/paste ──────────────────────────────────────────────────
    // xterm owns its selection (separate from the browser), so wire the
    // clipboard the way a real terminal does.
    const isMac = /mac/i.test(navigator.platform || navigator.userAgent);
    const copySelection = () => {
      const sel = term.getSelection();
      if (sel && navigator.clipboard) {
        navigator.clipboard.writeText(sel).catch(() => {});
        return true;
      }
      return false;
    };
    // Paste through term.paste(): it honors the app's bracketed-paste mode —
    // wrapping in ESC[200~ … ESC[201~ ONLY when the running program (Claude
    // Code, vim, modern bash) enabled it, and sending raw otherwise. A plain
    // shell that never turned the mode on gets clean text with no "^[[200~".
    const pasteText = (text: string) => {
      if (text) term.paste(text);
    };
    const pasteFromClipboard = () => {
      navigator.clipboard?.readText().then(pasteText).catch(() => {});
    };

    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      const key = e.key.toLowerCase();

      // Copy: Cmd+C (mac) · Ctrl+Shift+C · Ctrl+Insert
      const copyCombo =
        (isMac && e.metaKey && key === "c") ||
        (e.ctrlKey && e.shiftKey && key === "c") ||
        (e.ctrlKey && key === "insert");
      if (copyCombo) {
        if (copySelection()) {
          e.preventDefault();
          return false;
        }
        return true;
      }

      // Non-mac Ctrl+C with a selection copies (then clears) instead of SIGINT;
      // with no selection it falls through as the interrupt.
      if (!isMac && e.ctrlKey && !e.shiftKey && !e.altKey && key === "c") {
        if (term.hasSelection()) {
          copySelection();
          term.clearSelection();
          e.preventDefault();
          return false;
        }
      }
      // Paste combos (Cmd+V · Ctrl+Shift+V · Shift+Insert) are left to the
      // browser's native `paste` event, intercepted below — that keeps a single
      // code path and lets us block xterm's own bracketed-paste handler.
      return true;
    });

    // Intercept the browser paste in the capture phase so it runs before (and
    // instead of) xterm's built-in bracketed-paste handler. Covers every paste
    // gesture the browser recognizes, including Shift+Insert.
    const onPaste = (e: ClipboardEvent) => {
      e.preventDefault();
      e.stopImmediatePropagation();
      pasteText(e.clipboardData?.getData("text") ?? "");
    };
    host.addEventListener("paste", onPaste, true);

    // Right-click: paste (or copy an active selection); Shift+right-click keeps
    // the browser menu. Middle-click pastes (Linux primary-selection habit).
    const onContextMenu = (e: MouseEvent) => {
      if (e.shiftKey) return;
      e.preventDefault();
      if (term.hasSelection()) copySelection();
      else pasteFromClipboard();
    };
    const onAuxClick = (e: MouseEvent) => {
      if (e.button === 1) {
        e.preventDefault();
        pasteFromClipboard();
      }
    };
    host.addEventListener("contextmenu", onContextMenu);
    host.addEventListener("auxclick", onAuxClick);

    const transport = attach({
      onOutput: (bytes) => term.write(bytes),
      onStatus: (status) => {
        if (!disposed) onStatus?.(status);
      },
      // NOTE: deliberately NO terminal reset on reconnect. tmux's replay only
      // repaints CONTENT — terminal modes (scroll regions, cursor modes) are
      // accumulated from the live stream and must survive reconnects; a reset
      // wipes them while tmux keeps assuming they're set, garbling the screen.
      // The agreed grid for this session (the driver's size). The local grid
      // MUST always reconcile with this — any lasting xterm-vs-tmux size
      // mismatch renders as doubled status bars / ghost rows.
      onSize: (cols, rows) => {
        if (disposed) return;
        grid = { cols, rows };
        const matchesVote = (v: { cols: number; rows: number } | null) =>
          !!v && v.cols === cols && v.rows === rows;
        isDriver = matchesVote(lastVote) || matchesVote(lastSent);
        if (isDriver) {
          // Takeover (or stale echo): if the terminal isn't already at OUR
          // latest intended size, snap to the agreed grid at full font. When a
          // newer local fit is mid-flight (drag), term already equals lastVote
          // — leave it; its echo is coming.
          if (
            !lastVote ||
            term.cols !== lastVote.cols ||
            term.rows !== lastVote.rows
          ) {
            if (term.options.fontSize !== BASE_FONT)
              term.options.fontSize = BASE_FONT;
            try {
              term.resize(cols, rows);
            } catch {
              // renderer not ready
            }
          }
        } else {
          applyGrid();
        }
      },
      // Deliberate unmount close (incl. StrictMode double-mount) is not a
      // disconnect worth surfacing.
      onClose: () => {
        if (!disposed) onStatus?.("disconnected");
      },
    });

    const dataSub = term.onData((data) => {
      transport.sendInput(new TextEncoder().encode(data));
    });

    // First fit once the renderer has its dimensions; then follow the panel.
    // (Deferred out of the observation callback so resizing the terminal —
    // which changes the observed element — can't re-enter the observer.)
    requestAnimationFrame(refit);
    const observer = new ResizeObserver(() => requestAnimationFrame(refit));
    observer.observe(host);
    term.focus();

    return () => {
      disposed = true;
      clearTimeout(voteTimer);
      observer.disconnect();
      host.removeEventListener("paste", onPaste, true);
      host.removeEventListener("contextmenu", onContextMenu);
      host.removeEventListener("auxclick", onAuxClick);
      dataSub.dispose();
      transport.close();
      term.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Shell owns the padding + surface color (mirroring the xterm theme exactly
  // so the inset is indistinguishable); the host stays a clean content box the
  // FitAddon can measure without padding skew.
  return (
    <div
      className="terminal-shell"
      style={{ background: terminalTheme(tokens).background }}
    >
      <div ref={hostRef} className="terminal-host" />
    </div>
  );
}
