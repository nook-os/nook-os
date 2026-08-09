// The online dot (MAIN-163 AC-1). Rendered only for someone a `presence` frame
// said is online: absent means UNKNOWN, not offline — the server sends edges
// and no roster, so a grey "offline" dot would be a claim we cannot make.
import React from "react";

export function PresenceDot({
  online,
  label = "online",
}: {
  online: boolean;
  /** What a screen reader hears. Callers naming a person ("Ada is online")
   *  should say so; a row that already reads the name can keep the default. */
  label?: string;
}) {
  if (!online) return null;
  return <span className="chat-presence-dot" role="img" aria-label={label} title={label} />;
}
