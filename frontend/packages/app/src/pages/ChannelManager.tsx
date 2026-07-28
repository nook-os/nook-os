// The Discord-style channel-management modal (MAIN-94). Admin-only — the Chat
// page only mounts it for a tenant owner/admin, and the chat service refuses a
// non-admin's write anyway. It lists the tenant's channels (archived included),
// creates, renames, and archives/unarchives; every change invalidates the
// shared ["chat","channels"] query so the sidebar and this list both refresh
// without a reload.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Archive, ArchiveRestore, Pencil, Plus, Trash2, X } from "lucide-react";
import {
  createCategory,
  createChannel,
  deleteCategory,
  listCategories,
  listChannels,
  placeChannel,
  renameCategory,
  updateChannel,
  type ChatCategory,
  type ChatChannel,
} from "@nookos/api";
import { askConfirm, askText, notify } from "../dialogs";
import { ContextMenuRegion, type ContextMenuItem } from "../contextMenu";

export function ChannelManager({ onClose }: { onClose: () => void }) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Who the new channel belongs to: "tenant" (your team, default) or "org"
  // (shared across every tenant under your org — MAIN-112 AC-4).
  const [owner, setOwner] = useState<"tenant" | "org">("tenant");

  const q = useQuery({
    // A superset key of the sidebar's ["chat","channels"]: invalidating that
    // prefix refreshes BOTH this modal and the sidebar in one call.
    queryKey: ["chat", "channels", "manage"],
    queryFn: () => listChannels(true),
  });
  const channels = q.data ?? [];
  const active = channels.filter((c) => !c.archived);
  const archived = channels.filter((c) => c.archived);

  // Categories (MAIN-179): create/rename/delete + assign a channel's category.
  const [catName, setCatName] = useState("");
  const catsQuery = useQuery({ queryKey: ["chat", "categories"], queryFn: listCategories });
  const categories = [...(catsQuery.data ?? [])].sort((a, b) => a.position - b.position);

  // Invalidate the whole chat prefix so the sidebar (channels + categories) and
  // this modal both refresh after any change.
  const refresh = () => qc.invalidateQueries({ queryKey: ["chat"] });

  const create = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError("A channel needs a name.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await createChannel(trimmed, owner);
      setName("");
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const rename = async (c: ChatChannel) => {
    const next = await askText({
      title: "Rename channel",
      label: "Name",
      value: c.name,
    });
    if (!next || next === c.name) return;
    try {
      await updateChannel(c.id, { name: next });
      refresh();
    } catch (e) {
      await notify("Could not rename", e instanceof Error ? e.message : String(e));
    }
  };

  const setArchived = async (c: ChatChannel, value: boolean) => {
    try {
      await updateChannel(c.id, { archived: value });
      refresh();
    } catch (e) {
      await notify("Could not update", e instanceof Error ? e.message : String(e));
    }
  };

  const createCat = async () => {
    const trimmed = catName.trim();
    if (!trimmed) return;
    try {
      await createCategory(trimmed);
      setCatName("");
      refresh();
    } catch (e) {
      await notify("Could not create category", e instanceof Error ? e.message : String(e));
    }
  };

  const renameCat = async (cat: ChatCategory) => {
    const next = await askText({ title: "Rename category", label: "Name", value: cat.name });
    if (!next || next === cat.name) return;
    try {
      await renameCategory(cat.id, next);
      refresh();
    } catch (e) {
      await notify("Could not rename", e instanceof Error ? e.message : String(e));
    }
  };

  const deleteCat = async (cat: ChatCategory) => {
    const ok = await askConfirm({
      title: "Delete category",
      description: `"${cat.name}" will be removed. Its channels are NOT deleted — they become uncategorized.`,
      confirmLabel: "delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await deleteCategory(cat.id);
      refresh();
    } catch (e) {
      await notify("Could not delete", e instanceof Error ? e.message : String(e));
    }
  };

  // Assign a channel to a category (or `null` for uncategorized); land it at the
  // end of the target group, mirroring the sidebar's drop model.
  const assignCategory = async (c: ChatChannel, categoryId: string | null) => {
    const position =
      channels
        .filter((x) => (x.category_id ?? null) === categoryId)
        .reduce((m, x) => Math.max(m, x.position ?? 0), -1) + 1;
    try {
      await placeChannel(c.id, { category_id: categoryId, position });
      refresh();
    } catch (e) {
      await notify("Could not move", e instanceof Error ? e.message : String(e));
    }
  };

  // The same Rename + Archive/Unarchive right-click menu the sidebar offers
  // (MAIN-177). The sidebar never lists archived channels, so this modal is the
  // one place a channel's Unarchive item is reachable by right-click. No delete
  // (AC-4/NG-2); this modal is admin-only, so the items are admin-gated (AC-2).
  const menuItems = (c: ChatChannel): ContextMenuItem[] => [
    { label: "Rename…", onSelect: () => void rename(c) },
    {
      label: c.archived ? "Unarchive" : "Archive",
      onSelect: () => void setArchived(c, !c.archived),
    },
  ];

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal chan-manage"
        role="dialog"
        aria-label="Manage channels"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          Manage channels
          <button className="btn small icon chan-manage-close" onClick={onClose} aria-label="close">
            <X size={13} />
          </button>
        </div>
        <div className="modal-body">
          <div className="chan-manage-create">
            <input
              className="chan-input"
              value={name}
              placeholder="new-channel"
              aria-label="new channel name"
              onChange={(e) => {
                setName(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") void create();
              }}
            />
            <select
              className="chan-owner-select"
              value={owner}
              aria-label="channel owner"
              onChange={(e) => setOwner(e.target.value as "tenant" | "org")}
            >
              <option value="tenant">My team</option>
              <option value="org">My org</option>
            </select>
            <button className="btn small primary" onClick={create} disabled={busy}>
              <Plus size={12} /> create
            </button>
          </div>
          {error && <div className="chan-manage-error">{error}</div>}

          <div className="chan-manage-cats">
            <div className="chan-manage-subhdr faint small">Categories</div>
            <div className="chan-manage-create">
              <input
                className="chan-input"
                value={catName}
                placeholder="new-category"
                aria-label="new category name"
                onChange={(e) => setCatName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void createCat();
                }}
              />
              <button className="btn small primary" onClick={createCat}>
                <Plus size={12} /> add category
              </button>
            </div>
            {categories.length === 0 ? (
              <div className="faint small">No categories yet.</div>
            ) : (
              categories.map((cat) => (
                <div key={cat.id} className="chan-manage-row">
                  <span className="chan-manage-name">{cat.name}</span>
                  {cat.owner_type === "org" && (
                    <span className="chat-channel-org" title="Shared across your org">
                      org
                    </span>
                  )}
                  <button
                    className="btn small"
                    onClick={() => renameCat(cat)}
                    title="rename category"
                  >
                    <Pencil size={11} />
                  </button>
                  <button
                    className="btn small"
                    onClick={() => deleteCat(cat)}
                    title="delete category"
                  >
                    <Trash2 size={11} />
                  </button>
                </div>
              ))
            )}
          </div>

          <div className="chan-manage-list">
            {q.isLoading ? (
              <div className="faint small">Loading…</div>
            ) : active.length === 0 ? (
              <div className="faint small">No channels yet.</div>
            ) : (
              active.map((c) => (
                <ContextMenuRegion
                  key={c.id}
                  items={() => menuItems(c)}
                  style={{ display: "contents" }}
                >
                  <div className="chan-manage-row">
                    <span className="chat-channel-hash">#</span>
                    <span className="chan-manage-name">{c.name}</span>
                    <select
                      className="chan-owner-select"
                      aria-label={`category for ${c.name}`}
                      value={c.category_id ?? ""}
                      onChange={(e) => void assignCategory(c, e.target.value || null)}
                    >
                      <option value="">(uncategorized)</option>
                      {categories.map((cat) => (
                        <option key={cat.id} value={cat.id}>
                          {cat.name}
                        </option>
                      ))}
                    </select>
                    <button className="btn small" onClick={() => rename(c)} title="rename">
                      <Pencil size={11} />
                    </button>
                    <button
                      className="btn small"
                      onClick={() => setArchived(c, true)}
                      title="archive"
                    >
                      <Archive size={11} />
                    </button>
                  </div>
                </ContextMenuRegion>
              ))
            )}
          </div>

          {archived.length > 0 && (
            <div className="chan-manage-archived">
              <div className="chan-manage-subhdr faint small">Archived</div>
              {archived.map((c) => (
                <ContextMenuRegion
                  key={c.id}
                  items={() => menuItems(c)}
                  style={{ display: "contents" }}
                >
                  <div className="chan-manage-row archived">
                    <span className="chat-channel-hash">#</span>
                    <span className="chan-manage-name">{c.name}</span>
                    <button
                      className="btn small"
                      onClick={() => setArchived(c, false)}
                      title="unarchive"
                    >
                      <ArchiveRestore size={11} /> restore
                    </button>
                  </div>
                </ContextMenuRegion>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
