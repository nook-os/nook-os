// The floating progress panel: what's running right now, bottom-right by
// default, draggable anywhere and remembered across reloads.
//
// Deliberately generic — anything that calls `useJobs().start()` shows up
// here, so cloning, worktrees and future long operations share one surface.
import React, { useCallback, useEffect, useRef, useState } from "react";
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

  const onPointerDown = useCallback(
    (e: React.PointerEvent) => {
      const el = ref.current;
      if (!el) return;
      const r = el.getBoundingClientRect();
      drag.current = { dx: e.clientX - r.left, dy: e.clientY - r.top };
      el.setPointerCapture(e.pointerId);
    },
    [],
  );

  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      const el = ref.current;
      if (!drag.current || !el) return;
      const r = el.getBoundingClientRect();
      // Keep it on screen no matter how enthusiastically it's flung.
      const nx = Math.min(
        Math.max(MARGIN, e.clientX - drag.current.dx),
        window.innerWidth - r.width - MARGIN,
      );
      const ny = Math.min(
        Math.max(MARGIN, e.clientY - drag.current.dy),
        window.innerHeight - r.height - MARGIN,
      );
      set(nx, ny);
    },
    [set],
  );

  const endDrag = useCallback((e: React.PointerEvent) => {
    drag.current = null;
    ref.current?.releasePointerCapture(e.pointerId);
  }, []);

  if (jobs.length === 0) return null;
  const running = jobs.filter((j) => j.state === "running").length;

  const style: React.CSSProperties =
    x === null || y === null
      ? { right: MARGIN, bottom: MARGIN }
      : { left: x, top: y };

  return (
    <div ref={ref} className="jobs-hud" style={style}>
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
