// MAIN-167. Two things carry the feature and are tested here: the item
// RESOLUTION (a registered region beats the contextual fallback, and a
// native-owned subtree suppresses the shared menu), and the Copy/Cut/Paste
// ENABLEMENT MATRIX — selection × editable × clipboard-permission — via the
// pure `fallbackItems`, so the load-bearing table is checked without a DOM.
// Plus the dismiss behaviors (Escape, click-away). jsdom only.
import React from "react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import {
  ContextMenuProvider,
  ContextMenuRegion,
  fallbackItems,
} from "./contextMenu";

afterEach(() => cleanup());

describe("fallbackItems — enablement matrix", () => {
  const bools = [false, true];
  for (const hasSelection of bools) {
    for (const isEditable of bools) {
      for (const canReadClipboard of bools) {
        it(`selection=${hasSelection} editable=${isEditable} clipboard=${canReadClipboard}`, () => {
          const items = fallbackItems({
            hasSelection,
            isEditable,
            canReadClipboard,
          });
          const byLabel = (l: string) => items.find((i) => i.label === l);

          // Copy is always offered; enabled only when there is a selection.
          expect(byLabel("Copy")).toBeTruthy();
          expect(!!byLabel("Copy")!.disabled).toBe(!hasSelection);

          if (!isEditable) {
            // Non-editable target → Copy only, no Cut/Paste.
            expect(items).toHaveLength(1);
            expect(byLabel("Cut")).toBeUndefined();
            expect(byLabel("Paste")).toBeUndefined();
            return;
          }

          // Editable target → Cut and Paste join.
          expect(items).toHaveLength(3);
          expect(!!byLabel("Cut")!.disabled).toBe(!hasSelection);

          const paste = byLabel("Paste")!;
          if (canReadClipboard) {
            expect(!!paste.disabled).toBe(false);
            expect(paste.hint).toBeUndefined();
          } else {
            // Clipboard refused → disabled with an explanatory hint, not a throw.
            expect(paste.disabled).toBe(true);
            expect(paste.hint).toBe("Clipboard access blocked");
          }
        });
      }
    }
  }
});

function renderApp() {
  return render(
    <MemoryRouter>
      <ContextMenuProvider>
        <ContextMenuRegion
          items={[{ label: "Region Action", onSelect: () => {} }]}
        >
          <div data-testid="in-region">inside a region</div>
        </ContextMenuRegion>
        <div data-testid="outside">outside any region</div>
        <div data-testid="native" data-ctxmenu-native="">
          <div data-testid="in-native">legacy island</div>
        </div>
      </ContextMenuProvider>
    </MemoryRouter>,
  );
}

describe("resolution", () => {
  it("a registered region beats the fallback", async () => {
    renderApp();
    fireEvent.contextMenu(screen.getByTestId("in-region"));
    expect(await screen.findByText("Region Action")).toBeTruthy();
    // The fallback's Copy is NOT shown — the region won.
    expect(screen.queryByText("Copy")).toBeNull();
  });

  it("falls back to Copy outside any region", () => {
    renderApp();
    fireEvent.contextMenu(screen.getByTestId("outside"));
    expect(screen.getByText("Copy")).toBeTruthy();
    expect(screen.queryByText("Region Action")).toBeNull();
  });

  it("suppresses the shared menu inside a native-owned subtree", () => {
    renderApp();
    fireEvent.contextMenu(screen.getByTestId("in-native"));
    expect(screen.queryByText("Copy")).toBeNull();
    expect(screen.queryByText("Region Action")).toBeNull();
  });
});

describe("submenus (MAIN-168)", () => {
  function renderSub(onDone: () => void) {
    return render(
      <MemoryRouter>
        <ContextMenuProvider>
          <ContextMenuRegion
            items={[
              {
                label: "Move to",
                children: [
                  { label: "Todo", onSelect: () => {} },
                  { label: "Done", onSelect: onDone },
                ],
              },
            ]}
          >
            <div data-testid="card">a card</div>
          </ContextMenuRegion>
        </ContextMenuProvider>
      </MemoryRouter>,
    );
  }

  it("opens a submenu on hover and activates a child", async () => {
    let done = false;
    renderSub(() => {
      done = true;
    });
    fireEvent.contextMenu(screen.getByTestId("card"));
    const parent = await screen.findByText("Move to");
    // Children are not shown until the parent row is hovered.
    expect(screen.queryByText("Done")).toBeNull();
    fireEvent.mouseEnter(parent.closest(".ctxmenu-subhost")!);
    const done1 = await screen.findByText("Done");
    fireEvent.click(done1);
    expect(done).toBe(true);
    // Activating a child closes the whole menu.
    expect(screen.queryByText("Move to")).toBeNull();
  });

  it("does not close the menu when the parent row itself is clicked", async () => {
    renderSub(() => {});
    fireEvent.contextMenu(screen.getByTestId("card"));
    const parent = await screen.findByText("Move to");
    fireEvent.click(parent);
    // The parent toggles its submenu open; the menu stays up.
    expect(await screen.findByText("Done")).toBeTruthy();
  });
});

describe("dismiss", () => {
  it("closes on Escape", async () => {
    renderApp();
    fireEvent.contextMenu(screen.getByTestId("in-region"));
    const menu = await screen.findByRole("menu");
    fireEvent.keyDown(menu, { key: "Escape" });
    expect(screen.queryByText("Region Action")).toBeNull();
  });

  it("closes on click-away", async () => {
    renderApp();
    fireEvent.contextMenu(screen.getByTestId("in-region"));
    await screen.findByText("Region Action");
    fireEvent.mouseDown(document.body);
    expect(screen.queryByText("Region Action")).toBeNull();
  });
});
