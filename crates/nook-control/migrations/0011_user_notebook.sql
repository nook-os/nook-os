-- The personal notebook (MAIN-66): person-owned folders + markdown notes,
-- encrypted at rest, invisible to operators. The FIRST person_id-owned resource
-- — NOT tenant/user scoped — so a person sees the same notebook signed into any
-- of their orgs.
--
-- Append-only and idempotent (CREATE TABLE IF NOT EXISTS): re-running converges.
-- Note bodies live in `content_enc` (AES-256-GCM ciphertext, written via the
-- vault); `title`, folder `name`, and the derived path are plaintext metadata
-- the server needs for the tree, listing, and search.

CREATE TABLE IF NOT EXISTS public.user_note_folders (
    id uuid PRIMARY KEY,
    -- The owning PERSON (0002_add_person_id), not a tenant/user row. One person
    -- correlates several users (one per tenant); the notebook follows the person.
    person_id uuid NOT NULL,
    -- Null = a root folder. A self-referential tree; deleting a folder reparents
    -- its children to this parent (handled in the route, not a cascade).
    parent_id uuid REFERENCES public.user_note_folders(id) ON DELETE SET NULL,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS user_note_folders_person_idx
    ON public.user_note_folders (person_id);

CREATE TABLE IF NOT EXISTS public.user_notes (
    id uuid PRIMARY KEY,
    person_id uuid NOT NULL,
    -- Null = a note at the notebook root. On folder delete these are reparented,
    -- never cascade-deleted, so SET NULL is the safe default if a folder vanishes
    -- by any other path.
    folder_id uuid REFERENCES public.user_note_folders(id) ON DELETE SET NULL,
    title text NOT NULL,
    -- The note body as vault ciphertext. A raw SELECT yields bytea, never text.
    content_enc bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS user_notes_person_idx
    ON public.user_notes (person_id);
CREATE INDEX IF NOT EXISTS user_notes_folder_idx
    ON public.user_notes (folder_id);
