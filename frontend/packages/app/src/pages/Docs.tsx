import React from "react";
import { Panel } from "@nookos/ui";

// In-app help. Kept intentionally short and honest about what exists today
// and what's a known limitation.
export function DocsPage() {
  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      <Panel title="Docs · how NookOS works">
        <div className="doc-body">
          <h2>The model: Git → Workspaces → Projects → Sessions</h2>
          <p>
            NookOS coordinates the developer tools you already use. <b>Git drives everything.</b>{" "}
            A <b>workspace</b> is a repository; it can exist on many <b>nodes</b> (machines) at
            once. Each checkout — the primary clone or a <b>git worktree</b> (an extra branch in a
            sibling folder sharing one <code>.git</code>) — is a <b>location</b> of that workspace.
            A <b>session</b> is a persistent, tmux-backed terminal running in one checkout.
          </p>

          <h2>Sessions are runtime-agnostic</h2>
          <p>
            A session is <i>not</i> "a bash session" — it runs whatever <b>runtime</b> you pick at
            creation: <code>bash</code>, <code>zsh</code>, or an AI agent like <code>claude</code>,{" "}
            <code>hermes</code>, <code>codex</code> — whatever that node has detected. The runtime
            picker lists AI runtimes first. Sessions survive refreshes, reconnects, and node
            restarts (tmux keeps them alive).
          </p>

          <h2>Starting work</h2>
          <p>
            Hit <b>+ New Work</b> (top bar). Pick what to work on — <b>clone a repo</b> (any
            GitHub/GitLab/Bitbucket/raw URL, with an optional saved SSH key for private repos),
            add a <b>new worktree</b>, spin up a <b>new empty project</b>, or open an{" "}
            <b>existing workspace</b> — choose the machine, choose the runtime, go. The work is the
            unit; the machine is just where it runs.
          </p>

          <h2>Kanban drives the work</h2>
          <p>
            The board is first-class and lives in the control plane. A task moves through{" "}
            <b>Triage → Todo → In Progress → Done</b>:
          </p>
          <ul>
            <li>
              <b>Triage</b>: nothing runs yet. <b>Dispatch</b> lets the resource-aware scheduler
              pick the best online node (most free memory, lowest load, fewest sessions) — or drag
              it to Todo and pick the node yourself.
            </li>
            <li>
              <b>Todo → In Progress</b>: <b>Start work</b> creates a worktree for the task's branch
              and opens a session in it.
            </li>
            <li>
              <b>In Progress → Done</b>: <b>Submit PR</b> records the PR (or generates a compare
              link) and moves it to Done.
            </li>
            <li>
              <b>Done</b>: <b>Prune worktree</b> removes the checkout when you're finished.
            </li>
          </ul>

          <h2>Machines & capacity</h2>
          <p>
            Every node reports live resources (CPU, memory, load, active sessions) on each
            heartbeat — see the bars on the <b>Nodes</b> page. That's what triage uses to schedule,
            and what you use to decide where to place work. Add a machine from Nodes → “+ add node”;
            it connects outbound (no inbound ports). Each node also has its own SSH key you can add
            as a deploy key to clone private repos.
          </p>
          <p>
            The “+ add node” modal hands you a one-line installer with a fresh join token:{" "}
            <code>curl -fLsS &lt;server&gt;/install.sh | sh -s -- --token …</code>. It downloads the
            agent <b>this control plane was built with</b>, joins, and can install a systemd
            service — so every machine in a self-hosted fleet runs the same version as the server.
            The same command without a token updates the binary in place and leaves the node's
            config alone; <code>nook update</code> on the machine does the same. Builds are served
            from <code>/dist/nook-&lt;os&gt;-&lt;arch&gt;</code>; a platform the server wasn't built
            for shows as “not built”.
          </p>

          <h2>Secrets</h2>
          <p>
            Each workspace has an <b>env vault</b> (AES-256-GCM encrypted at rest). Saving syncs the
            file to every online checkout — and a fresh clone or worktree of that workspace gets the
            secrets automatically.
          </p>

          <h2>Drive it from AI (MCP)</h2>
          <p>
            The MCP endpoint at <code>/mcp</code> exposes the same management surface: list/observe
            everything, clone repos, create projects, add worktrees, and drive the kanban lifecycle
            (dispatch, start work, submit PR, move). Joining nodes stays a human action.
          </p>

          <h2 className="warn-heading">Known limitation: port collisions</h2>
          <p>
            Multiple checkouts (worktrees) of the same app on one machine will fight over ports like
            443/3000. There's no automatic fix yet. On macOS you can alias loopback addresses
            (<code>ifconfig lo0 alias 127.0.0.2</code>) to give each checkout its own IP. On
            Linux/WSL the planned direction is a per-workspace reverse proxy (name-based routing) or
            having the node advertise the real bound port back to NookOS. For now, run one
            port-bound dev server per machine or assign ports manually.
          </p>
        </div>
      </Panel>
    </div>
  );
}
