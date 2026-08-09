// How much of this repo runs at once: the build ceiling and the review ceiling,
// in one row.
//
// They were three places before — the review ceiling inside the session policy,
// the build ceiling nowhere at all (settable only by `nook builds scale`), and
// the port declaration that decides how many sessions a node can host on its own
// tab. They are one question, so they are one surface, and this sits directly
// above the declaration on that tab.
//
// A composition and nothing else: each control owns its own fetch, write and
// refusal handling, which is what keeps `WorkspacePorts` renderable — and
// testable — without dragging two loop endpoints in with it.
import React from "react";
import { Panel } from "@nookos/ui";
import { BuildLoop } from "./BuildLoop";
import { ReviewLoop } from "./SessionPolicy";

export function WorkspaceScale({ workspaceId }: { workspaceId: string }) {
  return (
    <Panel title="Scale">
      <div className="loop-scales" data-testid="workspace-scale">
        <BuildLoop workspaceId={workspaceId} />
        <ReviewLoop workspaceId={workspaceId} />
      </div>
    </Panel>
  );
}
