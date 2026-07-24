import { describe, expect, it, vi } from "vitest";
import React, { act } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createRoot } from "react-dom/client";
import { DataList, dataListPhase, type DataColumn } from "./DataList";

interface Row {
  id: string;
  name: string;
}
const columns: DataColumn<Row>[] = [
  { key: "name", header: "Name", cell: (r) => r.name },
];
const rows: Row[] = [
  { id: "1", name: "alpha" },
  { id: "2", name: "beta" },
];
const el = (props: Record<string, unknown>) =>
  React.createElement(DataList<Row>, {
    columns,
    rowKey: (r: Row) => r.id,
    ...props,
  } as never);

describe("dataListPhase", () => {
  it("shows rows as soon as there are any, even mid-load", () => {
    expect(dataListPhase({ loading: false, count: 2, filtered: false })).toBe("rows");
    expect(dataListPhase({ loading: true, count: 2, filtered: true })).toBe("rows");
  });

  it("distinguishes an empty log from a search that found nothing", () => {
    expect(dataListPhase({ loading: false, count: 0, filtered: false })).toBe("empty");
    expect(dataListPhase({ loading: false, count: 0, filtered: true })).toBe("no-results");
  });

  it("shows a loading state only before the first rows arrive", () => {
    expect(dataListPhase({ loading: true, count: 0, filtered: false })).toBe("loading");
  });
});

describe("DataList render", () => {
  it("renders a cell for every row", () => {
    const html = renderToStaticMarkup(el({ rows }));
    expect(html).toContain("alpha");
    expect(html).toContain("beta");
  });

  it("shows the empty message when unfiltered with no rows", () => {
    const html = renderToStaticMarkup(
      el({ rows: [], empty: "Nothing here yet." }),
    );
    expect(html).toContain("Nothing here yet.");
    expect(html).not.toContain("No matches.");
  });

  it("shows the no-results message when a search returns nothing", () => {
    const html = renderToStaticMarkup(
      el({ rows: [], filtered: true, noResults: "No matches." }),
    );
    expect(html).toContain("No matches.");
    expect(html).not.toContain("Nothing here yet.");
  });

  it("renders 'load more' only when there is a next page", () => {
    expect(renderToStaticMarkup(el({ rows, hasMore: true }))).toContain("Load more");
    expect(renderToStaticMarkup(el({ rows, hasMore: false }))).not.toContain("Load more");
  });
});

describe("DataList load-more", () => {
  it("invokes onLoadMore when the control is clicked", async () => {
    (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
    const onLoadMore = vi.fn();
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    await act(async () => {
      root.render(el({ rows, hasMore: true, onLoadMore }));
    });
    const btn = container.querySelector<HTMLButtonElement>("button.data-list-more-btn");
    expect(btn).toBeTruthy();

    await act(async () => {
      btn!.click();
    });
    expect(onLoadMore).toHaveBeenCalledTimes(1);

    await act(async () => {
      root.unmount();
    });
    container.remove();
  });
});
