// Automation's own metadata on a card (MAIN-603).
//
// A producer — a CI job, a loop run, a nightly benchmark — writes a titled
// block of Markdown at a key it chose, and this renders it. The component is
// deliberately incurious: it does not read the body, does not special-case a
// key, and has no notion of report kinds. Everything it knows is `title`,
// `body_md` and `updated_at` (NG-1).
//
// Rendering goes through the SHARED `Markdown`, the one already rendering
// comments — which carries `remark-gfm` for tables and `rehype-sanitize` for
// everything a producer should not be able to put on the page. This is
// untrusted input written by automation, so there is no second renderer here
// and no relaxed schema (AC-6, NG-7).
import React from "react";
import { Markdown } from "@nookos/ui";
import type { TaskReport } from "@nookos/api";

export function TaskReports({ reports }: { reports: TaskReport[] }) {
  // No empty state: a card with no reports is the ordinary case, and a "no
  // reports yet" card in every sidebar would be a permanent advertisement for
  // a surface most people never write to.
  if (reports.length === 0) return null;
  return (
    <>
      {reports.map((r) => (
        <div key={r.id} className="side-card task-report">
          <div className="side-card-h task-report-h">
            <span className="task-report-title">{r.title}</span>
            <span className="faint small mono" title={`key: ${r.key}`}>
              {r.key}
            </span>
          </div>
          <Markdown src={r.body_md} />
          {/* The whole of "visibly stale" (AC-10): who last wrote this key and
              when. A report has no other signal that it is out of date — Nook
              cannot know, because it never reads the content. */}
          <div className="faint small task-report-foot">
            {r.author_name || "unknown"} · {new Date(r.updated_at).toLocaleString()}
          </div>
        </div>
      ))}
    </>
  );
}
