// The floating progress panel: what's running right now, bottom-right by
// default, draggable anywhere and remembered across reloads.
//
// Deliberately generic — anything that calls `useJobs().start()` shows up
// here, so cloning, worktrees and future long operations share one surface.
import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Check, GitBranch, Loader, TriangleAlert, X } from "lucide-react";
import { useHudPosition, useJobs, type Job } from "./jobs";

const MARGIN = 16;

function elapsed(job: Job): string {
  const end = job.finishedAt ?? Date.now();
  const secs = Math.max(0, Math.round((end - job.startedAt) / 1000));
  return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`;
}

function JobRow({ job }: { job: Job }) {
  const navigate = useNavigate();
  const dismiss = useJobs((s) => s.dismiss);
  const [, force] = useState(0);

  // Keep the elapsed counter honest while the job runs.
  useEffect(() => {
    if (job.state !== "running") return;
    const id = window.setInterval(() => force((n) => n + 1), 1000);
    return () => window.clearInterval(id);
  }, [job.state]);

  const icon =
    job.state === "running" ? (
      <Loader size={13} className="spin" />
    ) : job.state === "done" ? (
      <Check size={13} className="ok" />
    ) : (
      <TriangleAlert size={13} className="err" />
    );

  return (
    <div
      className={`job-row ${job.state}${job.href && job.state === "done" ? " clickable" : ""}`}
      onClick={() => {
        if (job.href && job.state === "done") navigate(job.href);
      }}
      title={job.message ?? job.label}
    >
      <span className="job-icon">{icon}</span>
      <span className="job-label">
        {job.label}
        {job.message && job.state !== "running" && (
          <span className="job-message">{job.message}</span>
        )}
      </span>
      <span className="job-time">{elapsed(job)}</span>
      <button
        className="job-dismiss"
        title="dismiss"
        onClick={(e) => {
          e.stopPropagation();
          dismiss(job.id);
        }}
      >
        <X size={11} />
      </button>
    </div>
  );
}

export function JobsHud() {
  const jobs = useJobs((s) => s.jobs);
  const clearFinished = useJobs((s) => s.clearFinished);
  const { x, y, set } = useHudPosition();
  const ref = useRef<HTMLDivElement>(null);
  const drag = useRef<{ dx: number; dy: number } | null>(null);

  // Finished jobs linger briefly, then tidy themselves away.
  useEffect(() => {
    const stale = jobs.filter(
      (j) => j.state !== "running" && j.finishedAt && Date.now() - j.finishedAt > 30_000,
    );
    if (stale.length === 0) return;
    const id = window.setTimeout(clearFinished, 1000);
    return () => window.clearTimeout(id);
  }, [jobs, clearFinished]);

  // Dragging writes to the DOM directly and only commits to the store on
  // release. Going through React state (let alone the persisted store) on
  // every pointermove meant a re-render and a localStorage write per pixel,
  // which is what made this feel like sludge.
  const onPointerDown = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    const el = ref.current;
    if (!el || e.button !== 0) return;
    const r = el.getBoundingClientRect();
    drag.current = { dx: e.clientX - r.left, dy: e.clientY - r.top };
    // Capture on the element that owns the handlers, or the retargeted moves
    // never reach us and the drag "sticks" the moment you leave the header.
    // Never let a capture failure abort the rest of the setup — the drag
    // still works without it, just with a smaller tracking area.
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // pointer already released, or a synthetic event
    }
    // Anchor to left/top before moving; the default position uses right/bottom.
    el.style.left = `${r.left}px`;
    el.style.top = `${r.top}px`;
    el.style.right = "auto";
    el.style.bottom = "auto";
    el.classList.add("dragging");
    e.preventDefault();
  }, []);

  const onPointerMove = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    const el = ref.current;
    if (!drag.current || !el) return;
    // Keep it on screen no matter how enthusiastically it's flung.
    const nx = Math.min(
      Math.max(MARGIN, e.clientX - drag.current.dx),
      window.innerWidth - el.offsetWidth - MARGIN,
    );
    const ny = Math.min(
      Math.max(MARGIN, e.clientY - drag.current.dy),
      window.innerHeight - el.offsetHeight - MARGIN,
    );
    el.style.left = `${nx}px`;
    el.style.top = `${ny}px`;
  }, []);

  const endDrag = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      const el = ref.current;
      if (!drag.current || !el) return;
      drag.current = null;
      try {
        e.currentTarget.releasePointerCapture(e.pointerId);
      } catch {
        // never captured; nothing to release
      }
      el.classList.remove("dragging");
      // One store write, one persist, at the end.
      const r = el.getBoundingClientRect();
      set(Math.round(r.left), Math.round(r.top));
    },
    [set],
  );

  // Position is applied imperatively and only when it actually changes. A
  // `style` prop would be re-applied on every render — and the elapsed-time
  // tick renders once a second, which would yank the panel back mid-drag.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || drag.current) return;
    if (x === null || y === null) {
      el.style.left = "auto";
      el.style.top = "auto";
      el.style.right = `${MARGIN}px`;
      el.style.bottom = `${MARGIN}px`;
    } else {
      el.style.left = `${x}px`;
      el.style.top = `${y}px`;
      el.style.right = "auto";
      el.style.bottom = "auto";
    }
  }, [x, y, jobs.length]);

  if (jobs.length === 0) return null;
  const running = jobs.filter((j) => j.state === "running").length;

  return (
    <div ref={ref} className="jobs-hud">
      <div
        className="jobs-hud-header"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
      >
        <GitBranch size={12} />
        <span>
          {running > 0 ? `${running} running` : "finished"}
        </span>
        <button
          className="job-dismiss"
          title="clear finished"
          onClick={clearFinished}
          onPointerDown={(e) => e.stopPropagation()}
        >
          <X size={11} />
        </button>
      </div>
      <div className="jobs-hud-body">
        {jobs.map((j) => (
          <JobRow key={j.id} job={j} />
        ))}
      </div>
      {running > 0 && <div className="jobs-hud-bar" />}
    </div>
  );
}
