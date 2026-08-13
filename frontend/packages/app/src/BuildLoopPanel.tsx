// The build loop, on screen (MAIN-387).
//
// MAIN-385's switch, pin and concurrency were reachable only from `nook builds
// loop` and curl. Putting the controls on a tab is the easy half; the half this
// panel exists for is the SILENCE. A repo whose switch is on and whose board
// has ready cards can be doing nothing for four separate reasons — the tenant
// switch is off, the ceiling is reached, the last run is inside its hold, or
// the run that exists cannot be placed — and none of them was visible anywhere.
// `buildLoopWhy` decides which; this renders it.
//
// The controls are NOT gated further than the server gates them (AC-6). A
// refusal is shown in the server's own words, beside the control that was
// touched, because the alternative — a disabled button, or a click that
// silently does nothing — is how somebody concludes the feature is broken.
import React, { useState } from "react";
import { Link } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { TriangleAlert } from "lucide-react";
import { api, type Schemas } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { BuildLoop } from "./BuildLoop";
import { BuilderStrip } from "./BuilderStrip";
import {
  buildLoopSettingsKey,
  buildLoopWhy,
  isLiveRun,
  pinLabel,
  useBuildLoopSettings,
  useTenantLoopsEnabled,
  useWorkspaceBuilds,
  whyWords,
  type BuildLoopSettings,
  type BuildRunRow,
} from "./buildRuns";

/** The label the escalation ladder raises when a card stops being the loop's
 *  business — the claim reaper's cap, and a PR closed unmerged. */
const ESCALATION_LABEL = "needs-human-review";

/** The server's own words for a refused write. Its 400/403 names the field or
 *  the rule; any sentence guessed here would be a second, worse answer. */
function refusalText(error: unknown): string {
  const e = error as { error?: string } | undefined;
  return e?.error ?? JSON.stringify(error);
}

type Patch = Schemas["SetBuildLoopSettingsRequest"];

/** One write path for every control on this surface, so the switch, the pin and
 *  Mission Control's chip cannot handle a refusal three different ways. */
function useBuildLoopWrite(workspaceId: string) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const save = async (body: Patch) => {
    setBusy(true);
    setRefusal(null);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/build-loop-settings", {
      params: { path: { id: workspaceId } },
      body,
    });
    setBusy(false);
    if (error) {
      setRefusal(refusalText(error));
      return false;
    }
    queryClient.invalidateQueries({ queryKey: buildLoopSettingsKey(workspaceId) });
    // Enabling evaluates the repo immediately (MAIN-385 AC-6), so the run list
    // can change within a moment of the click — refresh it rather than waiting
    // out the poll and looking inert.
    queryClient.invalidateQueries({ queryKey: ["workspace-builds", workspaceId] });
    return true;
  };

  return { save, busy, refusal };
}

/** The on/off switch. A button rather than a checkbox because it carries a
 *  sentence, not a tick: "off" here means something specific about who raises
 *  runs for this repo, and the tooltip is where that lives. */
function Switch({
  settings,
  busy,
  onFlip,
}: {
  settings: BuildLoopSettings;
  busy: boolean;
  onFlip: (next: boolean) => void;
}) {
  const on = settings.enabled;
  return (
    <button
      className={`task-chip ${on ? "on" : ""}`}
      disabled={busy}
      aria-label={`build loop: ${on ? "on" : "off"}`}
      title={
        on
          ? "the control plane raises build runs for this repo by itself — click to turn off"
          : "this repo's cards are built only when somebody asks — click to turn on"
      }
      onClick={() => onFlip(!on)}
    >
      {on ? "on" : "off"}
    </button>
  );
}

/** The pin, over the nodes you own (AC-1).
 *
 *  A pin never fails over — a run waits queued while its node is dark rather
 *  than starting somewhere else — so `Auto` is the ordinary answer and is
 *  listed first as the default rather than as an empty option. A node that is
 *  pinned but not yours is still listed, or the pin could be seen and never
 *  cleared. */
function NodePin({
  settings,
  busy,
  onPick,
}: {
  settings: BuildLoopSettings;
  busy: boolean;
  onPick: (nodeId: string | null) => void;
}) {
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const mine = (nodes ?? []).filter(
    (n) =>
      (!!me?.person_id && n.owner_person_id === me.person_id) || n.id === settings.node_id,
  );
  return (
    <select
      className="input small"
      aria-label="build loop node"
      disabled={busy}
      value={settings.node_id ?? ""}
      onChange={(e) => onPick(e.target.value || null)}
    >
      <option value="">Auto — any of your eligible nodes</option>
      {mine.map((n) => (
        <option key={n.id} value={n.id}>
          {n.name}
        </option>
      ))}
    </select>
  );
}

/**
 * MAIN-239's failure mode, said out loud (AC-5).
 *
 * The tenant switch gates every loop, so a repo's own switch does nothing while
 * it is off — and the only thing that fixes it is on another page. That is
 * exactly the shape of problem a stuck queue makes invisible: work sits there
 * and nothing on screen says which switch is off.
 */
function TenantLoopsNotice() {
  return (
    <div className="bl-warn" data-testid="build-loop-tenant-off">
      <TriangleAlert size={13} />
      <span>
        Loops are off for this tenant, so nothing is raised for this repo whatever
        this switch says.
      </span>
      {/* The section id, not a guess at a heading: `?section=` is what
          `SectionedPage` reads, so this lands ON the switch rather than on
          whichever section Settings happens to open with. */}
      <Link className="btn small" to="/settings?section=automation">
        Settings → Loops
      </Link>
    </div>
  );
}

/** Cards the ladder handed to a person (AC-4). Flagged here rather than left to
 *  a label chip on the board: the board shows the label to whoever is already
 *  looking at the card, and the question this panel answers is "why is my repo
 *  not building" — for which "three of its cards are waiting on you" is the
 *  whole answer. The comment saying why is on the card, one click away. */
function Escalated({ workspaceId }: { workspaceId: string }) {
  const { data: tasks } = useQuery({
    queryKey: ["tasks", "escalated", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/tasks", {
          params: { query: { workspace: workspaceId, label: [ESCALATION_LABEL] } },
        })
      ).data ?? [],
  });
  if (!tasks || tasks.length === 0) return null;
  return (
    <div className="bl-warn" data-testid="build-loop-escalated">
      <TriangleAlert size={13} />
      <span>
        {tasks.length} card{tasks.length === 1 ? "" : "s"} escalated to a human — the
        loop will not pick {tasks.length === 1 ? "it" : "them"} up again until somebody
        does.
      </span>
      <span className="bl-escalated-list">
        {tasks.map((t) => (
          <Link
            key={t.id}
            className="btn small"
            to={`/board?task=${t.key ?? t.id}`}
            title={`${t.title} — read the escalation comment`}
          >
            {t.key ?? "card"}
          </Link>
        ))}
      </span>
    </div>
  );
}

/** The live state, and — when nothing is running — why (AC-2). */
function LiveState({
  workspaceId,
  settings,
  runs,
}: {
  workspaceId: string;
  settings: BuildLoopSettings | null;
  runs: BuildRunRow[] | undefined;
}) {
  const tenantLoops = useTenantLoopsEnabled();
  // Newest first, as the listing returns them.
  const live = (runs ?? []).filter((r) => isLiveRun(r.state));
  const newestRun = (runs ?? [])[0] ?? null;

  // The hold needs `updated_at` and `build_outcome`, which the listing does not
  // carry — the job's own record does, on the key every other run surface
  // already reads. Only worth asking when nothing is live: a repo with a run in
  // flight is not backing off.
  const { data: newestDetail } = useQuery({
    queryKey: ["job", newestRun?.id],
    enabled: !!newestRun && live.length === 0,
    queryFn: async () =>
      (
        await api.GET("/api/v1/jobs/{id}", {
          params: { path: { id: newestRun?.id as string } },
        })
      ).data ?? null,
  });

  const why = buildLoopWhy({ tenantLoops, settings, runs, newest: newestDetail });

  return (
    <div className="bl-live">
      {tenantLoops === false && <TenantLoopsNotice />}
      {live.map((r) => (
        <BuilderStrip key={r.id} run={r} workspaceId={workspaceId} />
      ))}
      {/* Always, not only when idle: with a run in flight the sentence is still
          the answer to "and why is nothing ELSE starting" — the ceiling, a card
          inside its hold, or the gate the queued run named. Except when the
          notice above is up, which says the same thing and carries the fix. */}
      {why.kind !== "loading" && why.kind !== "tenant-off" && (
        <div className="faint small" data-testid="build-loop-why">
          {whyWords(why)}
        </div>
      )}
    </div>
  );
}

/**
 * The Build loop tab: the switch, the pin, the concurrency, and what the loop
 * is actually doing with them.
 *
 * Concurrency is `BuildLoop`, unchanged and reused rather than reimplemented —
 * it is the same `build_max_replicas` column, and a second editor for one
 * column on two tabs is how the two start disagreeing about what `null` means.
 */
export function BuildLoopPanel({ workspaceId }: { workspaceId: string }) {
  const { data: settings } = useBuildLoopSettings(workspaceId);
  const { data: runs } = useWorkspaceBuilds(workspaceId);
  const { save, busy, refusal } = useBuildLoopWrite(workspaceId);

  return (
    <Panel title="Build loop">
      <div className="bl-body" data-testid="build-loop-panel">
        {!settings ? (
          <Empty>Reading this repo&rsquo;s build loop…</Empty>
        ) : (
          <>
            <div className="bl-controls">
              <Switch
                settings={settings}
                busy={busy}
                onFlip={(enabled) => void save({ enabled })}
              />
              <span className="faint small">node</span>
              <NodePin
                settings={settings}
                busy={busy}
                onPick={(node) => void save({ node })}
              />
              <Pill tone="dim" title="the pin auto-fired runs are placed against">
                {pinLabel(settings)}
              </Pill>
            </div>

            {refusal && (
              <div className="small err" data-testid="build-loop-settings-refusal">
                {refusal}
              </div>
            )}

            <LiveState workspaceId={workspaceId} settings={settings} runs={runs} />
            <Escalated workspaceId={workspaceId} />
          </>
        )}

        <div className="loop-scales">
          <BuildLoop workspaceId={workspaceId} />
        </div>
      </div>
    </Panel>
  );
}

/**
 * The same switch and pin, per repo, on Mission Control (AC-3) — with the
 * builder strip beside them (AC-8).
 *
 * Compact on purpose: this rides a repo header that already carries a name, a
 * remote and a rollup. What it must show is the two facts somebody scans for —
 * is this repo building itself, and where — and it must be flippable without
 * leaving the page, which is the whole point of it being here rather than a
 * link to the tab.
 */
export function MissionBuildLoop({ workspaceId }: { workspaceId: string }) {
  const { data: settings } = useBuildLoopSettings(workspaceId);
  const { data: runs } = useWorkspaceBuilds(workspaceId);
  const { save, busy, refusal } = useBuildLoopWrite(workspaceId);
  if (!settings) return null;
  const live = (runs ?? []).filter((r) => isLiveRun(r.state));
  return (
    // Clicks here are about the repo's loop, not about expanding the row the
    // header's own handler would toggle.
    <span
      className="bl-mission"
      data-testid={`mission-build-loop-${workspaceId}`}
      // Mission's repo header is itself a button that collapses the row, and it
      // listens for Space as well as click — so a flip from the keyboard would
      // otherwise both write the switch AND fold the repo away under it.
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <span className="faint small">build</span>
      <Switch settings={settings} busy={busy} onFlip={(enabled) => void save({ enabled })} />
      <span className="faint small" data-testid="mission-build-pin">
        {pinLabel(settings)}
      </span>
      {live[0] && <BuilderStrip run={live[0]} workspaceId={workspaceId} />}
      {refusal && (
        <span className="small err" data-testid="mission-build-refusal">
          {refusal}
        </span>
      )}
    </span>
  );
}
