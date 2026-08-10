// A chat session's surface (MAIN-502): the conversation with the agent running
// in that checkout, in place of the terminal.
//
// It is the shared `ChatView`, used AS IT IS — no new props, no variant of its
// own. That is the whole point of AC-4: the component already owns scrolling
// and follow-the-bottom, which is exactly what a terminal on a phone cannot do,
// and `variant="transcript"` (MAIN-499) is already the reading an agent
// exchange needs.
//
// The data lives on the server, never here. This component fetches, renders,
// and posts; it holds no message of its own, which is what makes a reload, a
// reconnect and a second device all show the same conversation (AC-5).
import React, { useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type SessionMessage } from "@nookos/api";
import { ChatView, type ChatViewMessage } from "@nookos/ui";
import { foldToolActivity, looksLikeMarkdown, stripAnsi } from "../loop";

/** The query key `live.ts` invalidates on a `session_message` nudge. */
export const sessionMessagesKey = (sessionId: string) =>
  ["session-messages", sessionId] as const;

export async function fetchSessionMessages(
  sessionId: string,
): Promise<SessionMessage[]> {
  return (
    (
      await api.GET("/api/v1/sessions/{id}/messages", {
        params: { path: { id: sessionId } },
      })
    ).data ?? []
  );
}

/** The permission the agent is blocked on right now, if any.
 *
 *  The LAST unanswered one, not the first: the runtime asks about one tool at a
 *  time, so a `decision`-less row that is not the newest is one whose answer we
 *  never recorded — a node that died holding it. Offering buttons for that
 *  would address an agent that is not there. */
export function outstandingPermission(
  messages: SessionMessage[],
): SessionMessage | null {
  const pending = messages.filter(
    (m) => m.role === "permission" && !m.decision && m.permission_request_id,
  );
  return pending.length ? pending[pending.length - 1] : null;
}

/**
 * The conversation, as chat messages.
 *
 * Shares `foldToolActivity` with the loop's transcript rather than repeating
 * it: a session and a run emit the same `· Bash` markers, so a ladder of
 * identical tool lines collapses into one activity entry on both surfaces. A
 * permission request is folded in as an ordinary message too (AC-6) — it IS a
 * turn in the conversation; what makes it answerable is the buttons beside the
 * composer, not a different kind of row.
 */
export function chatMessages(messages: SessionMessage[]): ChatViewMessage[] {
  const lines = messages.map((m) => ({
    id: m.id,
    // The ROLE is the author, exactly as the loop's mapping uses `source` —
    // which is what makes a run of agent narration group under one header.
    // A permission request is the agent speaking, because it is.
    source: m.role === "permission" ? "agent" : m.role,
    content:
      m.role === "permission" ? permissionPrompt(m) : m.body,
    at: m.at,
  }));
  return foldToolActivity(lines).map((l) => ({
    id: l.id,
    authorId: l.source,
    authorName: l.source,
    body: stripAnsi(l.content),
    createdAt: l.at,
    markdown: looksLikeMarkdown(l.content),
    activity: l.steps,
  }));
}

/** What a permission request reads as in the log.
 *
 *  The tool and its own one-line summary, and the outcome once there is one —
 *  so scrolling back through a conversation shows what was asked AND what was
 *  decided, rather than a request that appears to have gone unanswered. */
function permissionPrompt(m: SessionMessage): string {
  const what = m.body?.trim() ? `${m.tool_name}: ${m.body.trim()}` : `${m.tool_name}`;
  const asked = `Permission needed — ${what}`;
  if (m.decision === "allow") return `${asked}\n\nAllowed.`;
  if (m.decision === "deny") return `${asked}\n\nDenied.`;
  return asked;
}

export function SessionChat({ sessionId }: { sessionId: string }) {
  const qc = useQueryClient();
  const { data: messages, isLoading } = useQuery({
    queryKey: sessionMessagesKey(sessionId),
    queryFn: () => fetchSessionMessages(sessionId),
  });
  const conversation = useMemo(() => messages ?? [], [messages]);
  const pending = useMemo(
    () => outstandingPermission(conversation),
    [conversation],
  );

  const send = useMutation({
    mutationFn: async (body: string) => {
      const res = await api.POST("/api/v1/sessions/{id}/messages", {
        params: { path: { id: sessionId } },
        body: { body },
      });
      // openapi-fetch reports HTTP failures in `error` rather than throwing, so
      // without this a refused send would look like a success and the message
      // would simply never appear.
      if (res.error) throw new Error("could not send that message");
      return res.data;
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: sessionMessagesKey(sessionId) }),
  });

  const decide = useMutation({
    mutationFn: async ({ requestId, allow }: { requestId: string; allow: boolean }) => {
      const res = await api.POST("/api/v1/sessions/{id}/permissions/{request_id}", {
        params: { path: { id: sessionId, request_id: requestId } },
        body: { allow },
      });
      if (res.error) throw new Error("could not answer that request");
      return res.data;
    },
    // Refetched either way: a 409 means somebody else answered it, and the
    // conversation is where the answer is.
    onSettled: () => qc.invalidateQueries({ queryKey: sessionMessagesKey(sessionId) }),
  });

  return (
    <div className="session-chat" data-testid="session-chat">
      <ChatView
        variant="transcript"
        messages={chatMessages(conversation)}
        onSend={(body) => send.mutate(body)}
        disabled={send.isPending || !!pending}
        placeholder={
          pending
            ? "answer the permission request above first…"
            : "tell the agent what you want — it runs in this checkout"
        }
        emptyLabel={
          isLoading
            ? "Loading…"
            : "Nothing yet — say what you want and the agent gets to work."
        }
        beforeComposer={
          // The request's CHOICES only. Its text is already in the log above as
          // a message (AC-6), so repeating it here would say the same thing
          // twice; what a reader still needs is the two buttons — and the same
          // shape the loop's ask uses, so the two surfaces answer alike.
          pending?.permission_request_id ? (
            <div className="lw-ask-choices" data-testid="permission-choices">
              <span className="faint small">
                {pending.tool_name} — allow this?
              </span>
              <button
                className="btn small primary"
                disabled={decide.isPending}
                onClick={() =>
                  decide.mutate({
                    requestId: pending.permission_request_id!,
                    allow: true,
                  })
                }
              >
                Allow
              </button>
              <button
                className="btn small"
                disabled={decide.isPending}
                onClick={() =>
                  decide.mutate({
                    requestId: pending.permission_request_id!,
                    allow: false,
                  })
                }
              >
                Deny
              </button>
            </div>
          ) : null
        }
      />
    </div>
  );
}
