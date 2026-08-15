// Choosing a repo, on the pagination contract (MAIN-606).
//
// Every workspace picker in the app used to be a `<select>` (or a radio list)
// built from the whole collection, which is exactly what the unbounded route
// existed for. With that route gone there is no "all of them" to render, so a
// picker is a SEARCH plus a page: typing narrows server-side through `q`, and
// the rest of the rows arrive through `loadMore()`.
//
// The paging affordance is the plain button `PagedListState` already exposes.
// Endless scroll is a separate child of the REST epic, and hand-rolling a
// second `IntersectionObserver` here to get ahead of it is what that card
// forbids — the button is also what a keyboard and a screen reader have
// instead, so it is the accessible path either way.
import React, { useState } from "react";
import { ChevronDown } from "lucide-react";
import type { WorkspaceDetail } from "@nookos/api";
import { SearchInput, useAnchoredMenu } from "@nookos/ui";
import { useWorkspace, useWorkspaces } from "./workspaces";

/** A row's trailing detail — where the repo lives, how it is set up. */
export type WorkspaceHint = (w: WorkspaceDetail) => React.ReactNode;

/** The list itself: search box, one page of rows, a way to reach the next. */
export function WorkspaceList({
  value,
  onChange,
  hint,
  noneLabel,
  autoFocus = false,
  maxHeight = 220,
}: {
  value: string;
  onChange: (id: string) => void;
  hint?: WorkspaceHint;
  /** Renders a leading row that clears the selection, e.g. "No workspace". */
  noneLabel?: string;
  autoFocus?: boolean;
  maxHeight?: number;
}) {
  const list = useWorkspaces();
  const rows = list.rows;

  return (
    <>
      <SearchInput
        onSearch={list.setSearch}
        placeholder="search repos…"
        ariaLabel="Search workspaces"
        autoFocus={autoFocus}
      />
      <div className="suggest-list" style={{ maxHeight }} role="listbox">
        {noneLabel && (
          <button
            type="button"
            role="option"
            aria-selected={value === ""}
            className={`suggest-item${value === "" ? " active" : ""}`}
            onClick={() => onChange("")}
          >
            <span className="muted">{noneLabel}</span>
          </button>
        )}
        {rows.length === 0 && !list.loading && (
          <div className="empty" style={{ height: "auto", padding: 14 }}>
            {list.filtered ? "no workspace matches" : "no workspace yet"}
          </div>
        )}
        {rows.map((w) => (
          <button
            key={w.id}
            type="button"
            role="option"
            aria-selected={value === w.id}
            className={`suggest-item${value === w.id ? " active" : ""}`}
            onClick={() => onChange(w.id)}
          >
            <span className="bright">{w.name}</span>
            {hint?.(w)}
          </button>
        ))}
        {list.hasMore && (
          <div className="data-list-more">
            <button
              type="button"
              className="data-list-more-btn"
              onClick={list.loadMore}
              disabled={list.loadingMore}
            >
              {list.loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </div>
    </>
  );
}

/**
 * The list behind a trigger button — the drop-in for a `<Select>` of
 * workspaces.
 *
 * The trigger's label comes from reading the SELECTED ROW by id, not from
 * finding it among the loaded rows: the selection usually predates the search
 * that is showing, and a picker that forgets what it is set to the moment you
 * type is worse than no label at all.
 */
export function WorkspacePicker({
  value,
  onChange,
  ariaLabel,
  placeholder = "pick a workspace",
  noneLabel,
  hint,
  className = "",
}: {
  value: string;
  onChange: (id: string) => void;
  ariaLabel?: string;
  placeholder?: string;
  noneLabel?: string;
  hint?: WorkspaceHint;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const close = React.useCallback(() => setOpen(false), []);
  const { hostRef, portal } = useAnchoredMenu(open, close, {
    height: 300,
    matchWidth: true,
  });
  const selected = useWorkspace(value || null);

  const menu = portal(
    <WorkspaceList
      value={value}
      onChange={(id) => {
        onChange(id);
        setOpen(false);
      }}
      hint={hint}
      noneLabel={noneLabel}
      autoFocus
    />,
    "ws-picker-menu",
  );

  return (
    <div ref={hostRef} className={`sel ${className}`}>
      <button
        type="button"
        className={`sel-trigger${open ? " open" : ""}`}
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className="sel-label">
          {value ? (selected?.name ?? "…") : (noneLabel ?? placeholder)}
        </span>
        <ChevronDown size={12} className="sel-caret" />
      </button>
      {menu}
    </div>
  );
}
