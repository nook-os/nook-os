// Typed WebSocket helpers for the two live channels: /ws/ui (events) and
// /ws/sessions/:id/attach (terminal bytes).
import type { components } from "./generated/schema";
import { socketUrl } from "./endpoint";

export type UiEvent = components["schemas"]["UiEvent"];
export type AttachServerMessage = components["schemas"]["AttachServerMessage"];
export type AttachClientMessage = components["schemas"]["AttachClientMessage"];

/** Persistent UI event stream with automatic reconnect + backoff. */
export function connectUiSocket(
  onEvent: (event: UiEvent) => void,
  onReconnect?: () => void,
): () => void {
  let closed = false;
  let socket: WebSocket | null = null;
  let backoff = 1000;
  let first = true;

  const open = () => {
    if (closed) return;
    socket = new WebSocket(socketUrl("/api/v1/ws/ui"));
    socket.onopen = () => {
      backoff = 1000;
      if (!first && onReconnect) onReconnect();
      first = false;
    };
    socket.onmessage = (e) => {
      try {
        onEvent(JSON.parse(e.data));
      } catch {
        // ignore malformed frames
      }
    };
    socket.onclose = () => {
      if (closed) return;
      setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };
  open();
  return () => {
    closed = true;
    socket?.close();
  };
}

export interface TerminalConnection {
  sendInput(bytes: Uint8Array): void;
  resize(cols: number, rows: number): void;
  close(): void;
}

const encoder = new TextEncoder();

function toB64(bytes: Uint8Array): string {
  let bin = "";
  bytes.forEach((b) => (bin += String.fromCharCode(b)));
  return btoa(bin);
}

function fromB64(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Attach to a session's terminal stream. Reconnects automatically (with
 *  backoff) on transport drops — e.g. a control-plane restart — replaying the
 *  screen via tmux on re-attach, so the terminal never needs a page refresh. */
export function attachSession(
  sessionId: string,
  handlers: {
    onOutput(bytes: Uint8Array): void;
    onStatus?(status: string): void;
    /** The agreed grid (the PTY follows the driver — whoever typed last). */
    onSize?(cols: number, rows: number): void;
    onClose?(): void;
  },
): TerminalConnection {
  let closed = false;
  let socket: WebSocket | null = null;
  let backoff = 500;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  const pending: string[] = [];
  /** Re-sent on every (re)connect so the new server-side viewer has our size. */
  let lastResize: string | null = null;

  const open = () => {
    if (closed) return;
    socket = new WebSocket(socketUrl(`/api/v1/ws/sessions/${sessionId}/attach`));
    socket.onmessage = (e) => {
      try {
        const msg: AttachServerMessage = JSON.parse(e.data);
        if (msg.type === "output") handlers.onOutput(fromB64(msg.data.data_b64));
        else if (msg.type === "status") handlers.onStatus?.(msg.data.status);
        else if (msg.type === "size") handlers.onSize?.(msg.data.cols, msg.data.rows);
      } catch {
        // ignore malformed frames
      }
    };
    socket.onopen = () => {
      backoff = 500;
      // Attaching marks the session watched/running server-side; reflect that
      // immediately so a prior "reconnecting" status clears.
      handlers.onStatus?.("running");
      if (lastResize) socket?.send(lastResize);
      for (const frame of pending.splice(0)) socket?.send(frame);
    };
    socket.onclose = () => {
      if (closed) {
        handlers.onClose?.();
        return;
      }
      handlers.onStatus?.("reconnecting");
      reconnectTimer = setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 10000);
    };
  };
  open();

  const send = (frame: AttachClientMessage) => {
    const json = JSON.stringify(frame);
    if (socket?.readyState === WebSocket.OPEN) socket.send(json);
    else if (pending.length < 256) pending.push(json);
  };

  return {
    sendInput(bytes) {
      send({ type: "input", data: { data_b64: toB64(bytes) } });
    },
    resize(cols, rows) {
      const frame: AttachClientMessage = {
        type: "resize",
        data: { cols, rows },
      };
      lastResize = JSON.stringify(frame);
      send(frame);
    },
    close() {
      closed = true;
      clearTimeout(reconnectTimer);
      socket?.close();
    },
  };
}

export function inputFromString(s: string): Uint8Array {
  return encoder.encode(s);
}
