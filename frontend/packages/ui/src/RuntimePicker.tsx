import React from "react";
import { Bot, type LucideIcon, Sparkles, SquareTerminal, Wand2 } from "lucide-react";

// AI runtimes lead (a session can BE an AI agent, not just a shell); shells
// follow. Order + icons come from here so every runtime picker matches.
const AI_RUNTIMES = ["claude", "hermes", "codex"];
const ICONS: Record<string, LucideIcon> = {
  claude: Bot,
  hermes: Wand2,
  codex: Sparkles,
};

export function orderRuntimes(available: string[]): string[] {
  const ai = AI_RUNTIMES.filter((r) => available.includes(r));
  const shells = available.filter((r) => !AI_RUNTIMES.includes(r));
  return [...ai, ...shells];
}

/** Default runtime: first shell (so new sessions open a shell), AI opt-in. */
export function defaultRuntime(available: string[]): string {
  const shells = available.filter((r) => !AI_RUNTIMES.includes(r));
  return shells[0] ?? available[0] ?? "bash";
}

export function RuntimePicker({
  available,
  value,
  onChange,
}: {
  available: string[];
  value: string;
  onChange: (runtime: string) => void;
}) {
  const runtimes = orderRuntimes(available.length ? available : ["bash"]);
  return (
    <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
      {runtimes.map((r) => {
        const Icon = ICONS[r] ?? SquareTerminal;
        const isAI = AI_RUNTIMES.includes(r);
        return (
          <button
            key={r}
            type="button"
            className={`runtime-chip${value === r ? " active" : ""}${isAI ? " ai" : ""}`}
            onClick={() => onChange(r)}
            title={isAI ? "AI runtime" : "shell"}
          >
            <Icon size={13} />
            {r}
          </button>
        );
      })}
    </div>
  );
}
