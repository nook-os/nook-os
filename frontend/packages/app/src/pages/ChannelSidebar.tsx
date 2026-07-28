// The chat sidebar's channel list, grouped Discord-style under collapsible
// category headers (MAIN-179). Categories come from the backend (MAIN-178);
// uncategorized channels sit in a "(uncategorized)" section on top. Collapse
// state persists in localStorage, mirroring the Backlog's `useCollapsed`.
//
// Admins can drag channels between/within groups and reorder categories — the
// same `@dnd-kit/core` model the board uses (drop into a container, land at the
// end), persisted via `placeChannel` / `reorderCategories`. Non-admins get the
// identical grouped, collapsible list with no drag handles (AC-4).
import React, { useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  DndContext,
  type DragEndEvent,
  PointerSensor,
  useDraggable,
  useDroppable,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { ChevronDown, ChevronRight } from "lucide-react";
import {
  placeChannel,
  reorderCategories,
  type ChatCategory,
  type ChatChannel,
} from "@nookos/api";
import { ContextMenuRegion, type ContextMenuItem } from "../contextMenu";

const COLLAPSE_KEY = "nook.chat.collapsedCategories";
const UNCATEGORIZED = "none";

/** Collapse state that survives a reload (AC-1), keyed by group key (a category
 *  id, or `"none"` for uncategorized). Mirrors the Backlog's `useCollapsed`. */
function useCollapsed(): [Set<string>, (key: string) => void] {
  const [ids, setIds] = useState<Set<string>>(() => {
    try {
      return new Set(JSON.parse(localStorage.getItem(COLLAPSE_KEY) ?? "[]"));
    } catch {
      return new Set();
    }
  });
  const toggle = (key: string) =>
    setIds((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      try {
        localStorage.setItem(COLLAPSE_KEY, JSON.stringify([...next]));
      } catch {
        /* a full/blocked storage must not break chat */
      }
      return next;
    });
  return [ids, toggle];
}

interface Group {
  /** `"none"` for uncategorized, else the category id. */
  key: string;
  name: string;
  /** null for the uncategorized group. */
  category: ChatCategory | null;
  channels: ChatChannel[];
}

/** Group channels by category: uncategorized first, then categories in
 *  `position` order; each group's channels in `position` (then name) order. A
 *  channel whose `category_id` points at an unknown category falls back to
 *  uncategorized so it never vanishes. Empty groups are shown to admins (drop
 *  targets) and hidden from non-admins. */
function buildGroups(
  channels: ChatChannel[],
  categories: ChatCategory[],
  canManage: boolean,
): Group[] {
  const known = new Set(categories.map((c) => c.id));
  const byCat = new Map<string, ChatChannel[]>();
  const uncategorized: ChatChannel[] = [];
  for (const ch of channels) {
    const cid = ch.category_id;
    if (cid && known.has(cid)) {
      const list = byCat.get(cid) ?? [];
      list.push(ch);
      byCat.set(cid, list);
    } else {
      uncategorized.push(ch);
    }
  }
  const sortChans = (a: ChatChannel, b: ChatChannel) =>
    (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name);
  uncategorized.sort(sortChans);
  for (const list of byCat.values()) list.sort(sortChans);

  const groups: Group[] = [];
  if (uncategorized.length > 0 || canManage) {
    groups.push({ key: UNCATEGORIZED, name: "(uncategorized)", category: null, channels: uncategorized });
  }
  for (const cat of [...categories].sort((a, b) => a.position - b.position)) {
    const chans = byCat.get(cat.id) ?? [];
    if (chans.length > 0 || canManage) {
      groups.push({ key: cat.id, name: cat.name, category: cat, channels: chans });
    }
  }
  return groups;
}

function ChannelButton({
  channel,
  selected,
  onSelect,
  canManage,
  menuItems,
}: {
  channel: ChatChannel;
  selected: boolean;
  onSelect: (id: string) => void;
  canManage: boolean;
  menuItems: (c: ChatChannel) => ContextMenuItem[];
}) {
  const unread = channel.unread_count ?? 0;
  const btn = (
    <button
      type="button"
      className={`chat-channel${selected ? " active" : ""}`}
      onClick={() => onSelect(channel.id)}
    >
      <span className="chat-channel-hash">#</span>
      {channel.name}
      {channel.owner_type === "org" && (
        <span className="chat-channel-org" title="Shared across your org">
          org
        </span>
      )}
      {unread > 0 && (
        <span className="chat-unread" aria-label={`${unread} unread`}>
          {unread > 99 ? "99+" : String(unread)}
        </span>
      )}
    </button>
  );
  // Admins get the right-click management menu (MAIN-177); a non-admin has no
  // region, so the app-wide Copy/Paste fallback shows instead.
  return canManage ? (
    <ContextMenuRegion items={() => menuItems(channel)} style={{ display: "contents" }}>
      {btn}
    </ContextMenuRegion>
  ) : (
    btn
  );
}

/** A draggable channel row (admin drag mode). */
function DraggableChannel(props: {
  channel: ChatChannel;
  selected: boolean;
  onSelect: (id: string) => void;
  menuItems: (c: ChatChannel) => ContextMenuItem[];
}) {
  const { attributes, listeners, setNodeRef, isDragging } = useDraggable({
    id: `ch:${props.channel.id}`,
  });
  return (
    <div
      ref={setNodeRef}
      className={`chat-channel-drag${isDragging ? " dragging" : ""}`}
      {...attributes}
      {...listeners}
    >
      <ChannelButton {...props} canManage />
    </div>
  );
}

/** A category group: a collapsible header (a drag handle for admins) over a
 *  droppable body that accepts channels (and, for real categories, other
 *  categories being reordered). */
function CategoryGroup({
  group,
  collapsed,
  onToggle,
  canManage,
  selectedId,
  onSelect,
  menuItems,
}: {
  group: Group;
  collapsed: boolean;
  onToggle: (key: string) => void;
  canManage: boolean;
  selectedId: string | null;
  onSelect: (id: string) => void;
  menuItems: (c: ChatChannel) => ContextMenuItem[];
}) {
  const { setNodeRef, isOver } = useDroppable({ id: `grp:${group.key}` });
  // Real categories are draggable by their header to reorder; uncategorized is
  // pinned on top and is a drop target only.
  const drag = useDraggable({ id: `cat:${group.key}`, disabled: !canManage || group.category === null });
  return (
    <div
      ref={canManage ? setNodeRef : undefined}
      className={`chat-cat-group${isOver ? " drop-over" : ""}`}
    >
      <div
        className={`chat-cat-head${canManage && group.category ? " draggable" : ""}`}
        ref={canManage && group.category ? drag.setNodeRef : undefined}
        {...(canManage && group.category ? { ...drag.attributes, ...drag.listeners } : {})}
      >
        <button
          type="button"
          className="chat-cat-toggle"
          aria-label={collapsed ? `expand ${group.name}` : `collapse ${group.name}`}
          aria-expanded={!collapsed}
          onClick={() => onToggle(group.key)}
        >
          {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
        </button>
        <span className="chat-cat-name">{group.name}</span>
        {group.category?.owner_type === "org" && (
          <span className="chat-channel-org" title="Shared across your org">
            org
          </span>
        )}
      </div>
      {!collapsed && (
        <div className="chat-cat-body">
          {group.channels.length === 0 ? (
            <div className="chat-cat-empty faint small">no channels</div>
          ) : canManage ? (
            group.channels.map((c) => (
              <DraggableChannel
                key={c.id}
                channel={c}
                selected={c.id === selectedId}
                onSelect={onSelect}
                menuItems={menuItems}
              />
            ))
          ) : (
            group.channels.map((c) => (
              <ChannelButton
                key={c.id}
                channel={c}
                selected={c.id === selectedId}
                onSelect={onSelect}
                canManage={false}
                menuItems={menuItems}
              />
            ))
          )}
        </div>
      )}
    </div>
  );
}

export function ChannelSidebar({
  channels,
  categories,
  selectedId,
  onSelect,
  canManage,
  menuItems,
  loading,
}: {
  channels: ChatChannel[];
  categories: ChatCategory[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  canManage: boolean;
  menuItems: (c: ChatChannel) => ContextMenuItem[];
  loading: boolean;
}) {
  const qc = useQueryClient();
  const [collapsed, toggle] = useCollapsed();
  const groups = useMemo(
    () => buildGroups(channels, categories, canManage),
    [channels, categories, canManage],
  );
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
  );

  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["chat", "channels"] });
    void qc.invalidateQueries({ queryKey: ["chat", "categories"] });
  };

  // Land a dragged channel at the END of its target group, or reorder a dragged
  // category to the target's slot — the board's drop-into-a-container model.
  const onDragEnd = (e: DragEndEvent) => {
    const activeId = String(e.active.id);
    const overId = e.over ? String(e.over.id) : null;
    if (!overId || !overId.startsWith("grp:")) return;
    const targetKey = overId.slice(4);

    if (activeId.startsWith("ch:")) {
      const chanId = activeId.slice(3);
      const targetCatId = targetKey === UNCATEGORIZED ? null : targetKey;
      const targetGroup = groups.find((g) => g.key === targetKey);
      const position =
        (targetGroup?.channels ?? []).reduce((m, c) => Math.max(m, c.position ?? 0), -1) + 1;
      const ch = channels.find((c) => c.id === chanId);
      if (!ch) return;
      const currentKey = ch.category_id ?? UNCATEGORIZED;
      // A no-op drop (same group, already last) needn't hit the server.
      if (currentKey === targetKey && ch === targetGroup?.channels.at(-1)) return;
      void placeChannel(chanId, { category_id: targetCatId, position }).then(refresh);
    } else if (activeId.startsWith("cat:")) {
      const catId = activeId.slice(4);
      if (targetKey === UNCATEGORIZED || targetKey === catId) return;
      const order = [...categories].sort((a, b) => a.position - b.position).map((c) => c.id);
      const from = order.indexOf(catId);
      const to = order.indexOf(targetKey);
      if (from < 0 || to < 0) return;
      order.splice(from, 1);
      order.splice(to, 0, catId);
      void reorderCategories(order).then(refresh);
    }
  };

  if (loading) return <div className="chat-channels-empty">Loading…</div>;
  if (channels.length === 0) return <div className="chat-channels-empty">No channels yet.</div>;

  // No categories yet → the classic flat list, no group headers (a tenant that
  // hasn't organized its channels sees exactly what it did before).
  if (categories.length === 0) {
    return (
      <>
        {channels.map((c) => (
          <ChannelButton
            key={c.id}
            channel={c}
            selected={c.id === selectedId}
            onSelect={onSelect}
            canManage={canManage}
            menuItems={menuItems}
          />
        ))}
      </>
    );
  }

  const list = groups.map((g) => (
    <CategoryGroup
      key={g.key}
      group={g}
      collapsed={collapsed.has(g.key)}
      onToggle={toggle}
      canManage={canManage}
      selectedId={selectedId}
      onSelect={onSelect}
      menuItems={menuItems}
    />
  ));

  // Non-admins get the same grouped list without a DndContext (read-only, AC-4).
  return canManage ? (
    <DndContext sensors={sensors} onDragEnd={onDragEnd}>
      {list}
    </DndContext>
  ) : (
    <>{list}</>
  );
}
