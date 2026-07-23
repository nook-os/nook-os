// "What your organization can see" — shown to every user, not just operators.
//
// The counterpart to the operator page, and the reason the policy model is
// worth having: a person can answer "what could my employer see?" without
// asking their employer. Silent widening is the failure mode that turns
// governance into betrayal, so this is stated plainly and in both directions —
// what they CAN see, and what they cannot.
import React from "react";
import { useQuery } from "@tanstack/react-query";
import { Eye, EyeOff, Lock } from "lucide-react";
import { api } from "@nookos/api";
import { Panel } from "@nookos/ui";

/** Things no policy can ever reveal. Stated as strongly as the code enforces. */
const NEVER = [
  "What is on your terminal — any keystroke, any output",
  "Your prompts, and what your agents reply",
  "The contents of your code",
  "Your secrets and credentials",
];

export function OrgVisibility() {
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const orgId = me?.capability?.org_id ?? null;

  const { data: policy } = useQuery({
    queryKey: ["org-policy", orgId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/operator/orgs/{id}/policy", {
          params: { path: { id: orgId! } },
        })
      ).data ?? [],
    enabled: !!orgId,
  });

  // Without policy.view this 403s, which is itself an answer: nothing is
  // shared. Rendering the guarantees alone is more honest than an error.
  const visible = (policy ?? []).filter((f) => f.enabled);
  const hidden = (policy ?? []).filter((f) => !f.enabled);

  return (
    <Panel title="What your organization can see">
      <div className="vis-wrap">
        <div className="vis-block never">
          <div className="vis-head">
            <Lock size={12} /> Never, under any setting
          </div>
          {NEVER.map((n) => (
            <div key={n} className="vis-row">
              <EyeOff size={11} /> {n}
            </div>
          ))}
          <div className="muted small vis-note">
            This is not a preference. There is no permission that grants it and
            no setting that changes it — session access is membership of your
            own tenant, checked separately from every role.
          </div>
        </div>

        {visible.length > 0 && (
          <div className="vis-block visible">
            <div className="vis-head">
              <Eye size={12} /> Your organization's operators can currently see
            </div>
            {visible.map((f) => (
              <div key={f.field} className="vis-row">
                <Eye size={11} /> {f.description}
              </div>
            ))}
          </div>
        )}

        <div className="vis-block">
          <div className="vis-head">
            <EyeOff size={12} /> They cannot see
          </div>
          {hidden.map((f) => (
            <div key={f.field} className="vis-row faint">
              <EyeOff size={11} /> {f.description}
            </div>
          ))}
          {hidden.length === 0 && policy && (
            <div className="faint small">Every optional field is currently shared.</div>
          )}
        </div>

        <div className="muted small">
          You are told whenever this changes, and every change is recorded with
          who made it and when.
        </div>
      </div>
    </Panel>
  );
}
