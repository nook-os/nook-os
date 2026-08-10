// A page made of findable sections, replacing the panel-grid soup.
//
// Settings and Operator both grew the same way: every new capability added one
// more panel to a grid, until finding anything meant scanning all of them and
// the layout answered no question about where a thing SHOULD live. This is the
// shared shell that replaces that — a grouped section nav on the left, one
// section's content on the right, and a finder that matches titles and
// keywords, because "where do I change the chime" should be typed, not scanned.
//
// One component for both pages on purpose: the next surface that grows past
// three panels gets this for free, and the two cannot drift into different
// navigation models.
//
// The selected section lives in the URL (`?section=`), so "the setting you
// want is under Automation" can be a LINK — which is the other half of "hard
// to find": being told where something is should land you on it.
import React from "react";
import { useSearchParams } from "react-router-dom";
import { SearchX } from "lucide-react";

export interface PageSection {
  /** Stable id, used in the URL. Renaming a title must not break links. */
  id: string;
  title: string;
  /** Nav group heading — "You", "Team", "Deployment"… Sections keep their
   *  registry order within a group. */
  group: string;
  /** What the finder matches besides the title. The words somebody would
   *  actually type: "chime" for notifications, "passkey" for security. */
  keywords?: string[];
  /** Blast-radius label — "team", "org", "fleet". Absence means personal.
   *  This is the honest answer to "who does this affect", carried on the nav
   *  itself so scope is visible BEFORE a section is opened. */
  badge?: string;
  render: () => React.ReactNode;
}

/** Which sections a finder term matches. Every whitespace-separated word must
 *  hit somewhere (title, group, badge or keywords), so "team loops" narrows
 *  rather than widens. Exported for its own tests: this is the whole search. */
export function matchSections(sections: PageSection[], term: string): PageSection[] {
  const words = term.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return sections;
  return sections.filter((s) => {
    const hay = [s.title, s.group, s.badge ?? "", ...(s.keywords ?? [])]
      .join(" ")
      .toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

export function SectionedPage({
  sections,
  placeholder = "find…",
  /** Rendered above the nav+content grid — the page's standing banner, e.g.
   *  Operator's "metadata, never content". */
  intro,
}: {
  sections: PageSection[];
  placeholder?: string;
  intro?: React.ReactNode;
}) {
  const [params, setParams] = useSearchParams();
  const [term, setTerm] = React.useState("");

  const visible = matchSections(sections, term);
  // The URL's pick if it survives the filter, else the first visible section —
  // so typing narrows the content along with the nav. The auto-pick is never
  // written back to the URL: only a click is a navigation.
  const selectedId = params.get("section");
  const active = visible.find((s) => s.id === selectedId) ?? visible[0] ?? null;

  // Below the medium breakpoint the nav is a horizontal strip that scrolls, so
  // a section late in the list sits off-screen with nothing saying it exists
  // (MAIN-497 AC-2). `nearest` scrolls only the ancestors that need it, which
  // is also why this is safe to run at every width: in the desktop rail an
  // already-visible item moves nothing, and the document never scrolls.
  const activeRef = React.useRef<HTMLButtonElement>(null);
  React.useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active?.id]);

  const pick = (id: string) => {
    const next = new URLSearchParams(params);
    next.set("section", id);
    // Replace, not push: flipping between sections is not history somebody
    // wants to back-button through one hop at a time.
    setParams(next, { replace: true });
  };

  // Group order follows first appearance in the registry, so the page author
  // decides it by ordering sections rather than by a second list to maintain.
  const groups: { name: string; items: PageSection[] }[] = [];
  for (const s of visible) {
    const g = groups.find((x) => x.name === s.group);
    if (g) g.items.push(s);
    else groups.push({ name: s.group, items: [s] });
  }

  return (
    <div className="spage-wrap">
      {intro}
      <div className="spage">
        <nav className="spage-nav" aria-label="sections">
          <input
            className="input spage-find"
            placeholder={placeholder}
            aria-label="find a section"
            value={term}
            onChange={(e) => setTerm(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setTerm("");
            }}
          />
          {/* One element holding every group and item, so the compact strip has
              a single box to turn into a scrolling row with the finder above it
              rather than beside it. `display: contents` above the breakpoint,
              so the rail is laid out exactly as it was before it existed. */}
          <div className="spage-list">
            {groups.map((g) => (
              <React.Fragment key={g.name}>
                <div className="spage-group">{g.name}</div>
                {g.items.map((s) => (
                  <button
                    key={s.id}
                    ref={active?.id === s.id ? activeRef : undefined}
                    className={`spage-item${active?.id === s.id ? " active" : ""}`}
                    aria-current={active?.id === s.id ? "true" : undefined}
                    onClick={() => pick(s.id)}
                  >
                    <span className="spage-item-title">{s.title}</span>
                    {s.badge && <span className="spage-badge">{s.badge}</span>}
                  </button>
                ))}
              </React.Fragment>
            ))}
            {visible.length === 0 && (
              <div className="spage-empty faint small">
                <SearchX size={12} /> nothing matches “{term}”
              </div>
            )}
          </div>
        </nav>
        <div className="spage-content">{active?.render()}</div>
      </div>
    </div>
  );
}
