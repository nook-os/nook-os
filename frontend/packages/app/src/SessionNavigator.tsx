// The session navigator — the left pane beside the terminal (MAIN-414).
//
// `/sessions/list` already inventories every session, but it is a PAGE: to use
// it you leave the terminal you are working in. This is the other half — a
// pane you keep open, that answers "where is the claude session on the api
// repo" without navigating away from anything.
//
// It is not a generic sidebar and should not read as one. It is window chrome:
// it sits inside the session view, it pushes the terminal aside rather than
// floating over it whenever there is room, and when it is shut a pull-tab
// stays welded to the edge so it is never lost to a keyboard shortcut nobody
// remembers.
//
// The rules worth getting wrong are in `sessionNav.ts`, tested without a
// browser: what a folder is (borrowed wholesale from the tab strip's grouping,
// so the two cannot disagree), what a search leaves standing, and when pinning
// beats a narrow window.
import React from "react";
import { useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Building2,
  Check,
  ChevronDown,
  ChevronRight,
  CircleDot,
  Loader2,
  PanelLeftClose,
  Pin,
  PinOff,
  Search,
  Server,
  SquareTerminal,
} from "lucide-react";
import { api, type MeResponse } from "@nookos/api";
import { useLive } from "./live";
import { useLiveTabs } from "./liveTabs";
import { useTenantSwitch } from "./TenantSwitcher";
import {
  clampWidth,
  filterFolders,
  navFolders,
  paneMode,
  parseNavPrefs,
  COMPACT_WIDTH,
  DEFAULT_NAV_PREFS,
  MAX_PANE_WIDTH,
  MIN_PANE_WIDTH,
  NAV_PREFS_KEY,
  type NavPrefs,
} from "./sessionNav";

/** Width, collapsed and pinned, stored against the PERSON rather than the
 *  browser (AC-6). A pane you arranged on your laptop is the pane you get on
 *  the desktop; `localStorage` would have made it per-machine, which for a
 *  layout you deliberately set is just losing it. */
function useNavPrefs() {
  const queryClient = useQueryClient();
  const { data: settings } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });
  // The local copy exists so dragging the edge is not a round-trip per pixel.
  const [local, setLocal] = React.useState<NavPrefs | null>(null);
  const stored = React.useMemo(
    () => parseNavPrefs(settings?.find((s) => s.key === NAV_PREFS_KEY)?.value),
    [settings],
  );
  const prefs = local ?? stored;

  const write = React.useCallback(
    (next: NavPrefs) => {
      setLocal(next);
      void api
        .PUT("/api/v1/settings/{key}", {
          params: { path: { key: NAV_PREFS_KEY } },
          body: { scope: "user", value: next },
        })
        .then(() => queryClient.invalidateQueries({ queryKey: ["settings"] }))
        // A pane width that failed to save is not worth a dialog; the pane is
        // still the size you dragged it to for this session.
        .catch(() => {});
    },
    [queryClient],
  );

  // False until the settings row has actually arrived. The pane renders
  // nothing until then, for the same reason `SessionsIndex` waits for its
  // session list: acting on a default you have not confirmed is wrong twice
  // over — the pane flashes at 260px before jumping to the width you set, and
  // a click landing in that window WRITES the default back over it.
  return { prefs, write, loaded: settings !== undefined };
}

function useViewportWidth(): number {
  const [w, setW] = React.useState(() =>
    typeof window === "undefined" ? 1600 : window.innerWidth,
  );
  React.useEffect(() => {
    const onResize = () => setW(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return w;
}

/** The team this pane is listing, and the way to change it.
 *
 *  Here as well as in the top bar because the pane is a standing claim about
 *  what you are looking at: "these are the sessions" is only true of one team,
 *  and a list with no owner named is the kind of thing you read wrong once and
 *  never trust again. */
function NavTeam() {
  const [open, setOpen] = React.useState(false);
  const ref = React.useRef<HTMLDivElement>(null);
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });

  React.useEffect(() => {
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, []);

  // `me.tenant` and not just `me`: unlike the top bar's switcher — which is
  // handed a loaded `MeResponse` by the shell — this pane fetches its own, so
  // it renders during the window where the answer is partial. Reading
  // `me.tenant.id` there throws, and a header that throws takes the whole pane
  // (and the terminal beside it) down with it.
  if (!me?.tenant) return <span className="nav-team-name faint">…</span>;
  return <NavTeamMenu me={me} open={open} setOpen={setOpen} innerRef={ref} />;
}

function NavTeamMenu({
  me,
  open,
  setOpen,
  innerRef,
}: {
  me: MeResponse;
  open: boolean;
  setOpen: (v: boolean) => void;
  innerRef: React.RefObject<HTMLDivElement>;
}) {
  // The top bar's switcher, not a second one — same model, same POST, same
  // re-scoping (AC-7).
  const { model, switchTo, busy } = useTenantSwitch(me);

  if (!model.isMenu) {
    return (
      <span className="nav-team-name" title="your team">
        <Building2 size={12} />
        {model.currentName}
      </span>
    );
  }
  return (
    <div className="nav-team" ref={innerRef}>
      <button
        className="nav-team-btn"
        disabled={busy}
        title={`switch team — ${model.options.length} to choose from`}
        onClick={() => setOpen(!open)}
      >
        <Building2 size={12} />
        <span className="nav-team-name">{model.currentName}</span>
        <ChevronDown size={11} />
      </button>
      {open && (
        <div className="nav-team-menu">
          {model.options.map((t) => (
            <button
              key={t.id}
              className={`nav-team-item${t.current ? " current" : ""}`}
              onClick={() => {
                setOpen(false);
                void switchTo(t.id);
              }}
            >
              <span className="check">{t.current ? <Check size={11} /> : null}</span>
              {t.name}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function SessionNavigator({ activeId }: { activeId?: string }) {
  const navigate = useNavigate();
  const { prefs, write, loaded } = useNavPrefs();
  const [term, setTerm] = React.useState("");
  const [shut, setShut] = React.useState<string[]>([]);
  const [dragWidth, setDragWidth] = React.useState<number | null>(null);
  const agentState = useLive((s) => s.agentState);
  const sessionStatus = useLive((s) => s.sessionStatus);
  const viewportWidth = useViewportWidth();

  // Every workspace, not the scoped one: the pane's job is finding a session
  // you have NOT got open, and scoping it would hide exactly those.
  const { tabs } = useLiveTabs({ allWorkspaces: true });

  const width = dragWidth ?? prefs.width;
  const mode = paneMode({
    pinned: prefs.pinned,
    viewportWidth,
    paneWidth: width,
  });
  // On a phone the pane covers the terminal, so leaving it open after a pick
  // would hide the thing you just asked for (MAIN-418 AC-1).
  const compact = viewportWidth <= COMPACT_WIDTH;

  // Drag the trailing edge. Tracked on the document so the pointer may leave
  // the 4px handle mid-drag — which it does, constantly — without the resize
  // stopping dead.
  const startResize = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = width;
    const onMove = (m: MouseEvent) =>
      setDragWidth(clampWidth(startWidth + (m.clientX - startX)));
    const onUp = (m: MouseEvent) => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      const final = clampWidth(startWidth + (m.clientX - startX));
      setDragWidth(null);
      // One write per drag, at the end — not one per mouse move.
      if (final !== prefs.width) write({ ...prefs, width: final });
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  };

  if (!loaded) return null;

  if (prefs.collapsed) {
    return (
      <button
        className="session-nav-tab"
        title="show sessions"
        aria-label="show the session navigator"
        onClick={() => write({ ...prefs, collapsed: false })}
      >
        <ChevronRight size={12} />
        <span className="session-nav-tab-label">sessions</span>
      </button>
    );
  }

  const folders = filterFolders(navFolders(tabs), term);
  const total = tabs.length;

  return (
    <nav
      className={`session-nav ${mode}`}
      style={{ width }}
      aria-label="session navigator"
    >
      <div className="session-nav-head">
        <NavTeam />
        <span style={{ flex: 1 }} />
        <button
          className={`session-nav-icon${prefs.pinned ? " on" : ""}`}
          title={
            prefs.pinned
              ? "unpin — the pane may float over the terminal when the window is narrow"
              : "pin — always keep the terminal beside the pane, however narrow the window"
          }
          aria-pressed={prefs.pinned}
          onClick={() => write({ ...prefs, pinned: !prefs.pinned })}
        >
          {prefs.pinned ? <Pin size={12} /> : <PinOff size={12} />}
        </button>
        <button
          className="session-nav-icon"
          title="hide the pane"
          aria-label="hide the session navigator"
          onClick={() => write({ ...prefs, collapsed: true })}
        >
          <PanelLeftClose size={12} />
        </button>
      </div>

      <div className="session-nav-find">
        <Search size={12} />
        <input
          value={term}
          placeholder="find a session…"
          aria-label="find a session"
          onChange={(e) => setTerm(e.target.value)}
        />
      </div>

      <div className="session-nav-tree">
        {folders.map((f) => {
          const collapsed = shut.includes(f.key);
          return (
            <div className="session-nav-folder" key={f.key}>
              <button
                className={`session-nav-folder-head${collapsed ? " collapsed" : ""}`}
                style={{ "--group-hue": f.hue } as React.CSSProperties}
                onClick={() =>
                  setShut((s) =>
                    s.includes(f.key)
                      ? s.filter((k) => k !== f.key)
                      : [...s, f.key],
                  )
                }
                title={collapsed ? `expand ${f.label}` : `collapse ${f.label}`}
              >
                {collapsed ? <ChevronRight size={11} /> : <ChevronDown size={11} />}
                <span className="session-nav-folder-name">{f.label}</span>
                <span className="session-nav-folder-count">{f.sessions.length}</span>
              </button>
              {!collapsed &&
                f.sessions.map((s) => {
                  const st = sessionStatus[s.id];
                  // `stopped` is deliberately absent: it is not dead, it is parked, and
          // opening it starts it again (MAIN-415 AC-6).
          const dead = st === "exited" || st === "error" || st === "killed";
                  const agent = dead ? undefined : agentState[s.id]?.state;
                  return (
                    <button
                      key={s.id}
                      className={`session-nav-item${s.id === activeId ? " active" : ""}`}
                      style={{ "--group-hue": f.hue } as React.CSSProperties}
                      title={`${s.name} · ${s.runtime}${s.nodeName ? ` · ${s.nodeName}` : ""}`}
                      onClick={() => {
                        navigate(`/sessions/${s.id}`);
                        if (compact) write({ ...prefs, collapsed: true });
                      }}
                    >
                      {agent === "running" ? (
                        <Loader2 size={11} className="spin running" />
                      ) : agent === "waiting" ? (
                        <CircleDot size={11} className="waiting" />
                      ) : (
                        <SquareTerminal size={11} className={dead ? "err" : "ok"} />
                      )}
                      <span className="session-nav-item-name">{s.name}</span>
                      <span className="session-nav-item-runtime">{s.runtime}</span>
                      {s.nodeName && (
                        <span className="session-nav-item-node" title={`machine: ${s.nodeName}`}>
                          <Server size={9} />
                          {s.nodeName}
                        </span>
                      )}
                    </button>
                  );
                })}
            </div>
          );
        })}
        {folders.length === 0 && (
          <div className="session-nav-empty faint small">
            {total === 0 ? "no running sessions" : `nothing matches “${term}”`}
          </div>
        )}
      </div>

      <div
        className="session-nav-resize"
        role="separator"
        aria-label="resize the session navigator"
        aria-valuenow={width}
        aria-valuemin={MIN_PANE_WIDTH}
        aria-valuemax={MAX_PANE_WIDTH}
        onMouseDown={startResize}
      />
    </nav>
  );
}

export { DEFAULT_NAV_PREFS };

/**
 * The session view, with the navigator welded to its left edge.
 *
 * A wrapper rather than three copies of the same two elements: `SessionPage`,
 * the empty state and the inventory page all render `.session-view`, and a
 * pane that appeared on one of them and not the others would read as a bug in
 * whichever one you noticed second.
 *
 * Push vs overlay is CSS on the pane itself, not a layout branch here — the
 * pushed pane is a flex item that takes its width from the flow, the overlaid
 * one is taken out of it and floats. So the content markup is identical in
 * both modes and nothing re-mounts when the window is resized past the
 * threshold; the terminal keeps its scrollback.
 */
export function SessionShell({
  activeId,
  children,
}: {
  activeId?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="session-view">
      <SessionNavigator activeId={activeId} />
      <div className="session-stack">{children}</div>
    </div>
  );
}
