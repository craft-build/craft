import type { Tokens } from "../theme";

export type PillState = "thinking" | "running" | "waiting" | "done" | "failed";

const LABEL: Record<PillState, string> = {
  thinking: "thinking",
  running: "running",
  waiting: "waiting",
  done: "done",
  failed: "failed",
};

function pillColor(state: PillState, t: Tokens): string {
  switch (state) {
    case "thinking":
      return t.accentSecondary;
    case "running":
      return t.info;
    case "waiting":
      return t.warning;
    case "done":
      return t.success;
    case "failed":
      return t.danger;
  }
}

export function StatusPill({ state, t }: { state: PillState; t: Tokens }) {
  const color = pillColor(state, t);
  const pulse = state === "thinking" || state === "running";
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 6,
        padding: "2px 9px",
        borderRadius: "var(--radius-pill)",
        background: `color-mix(in oklch, ${color} 14%, transparent)`,
        border: `1px solid color-mix(in oklch, ${color} 40%, transparent)`,
        color,
        fontFamily: "var(--font-mono)",
        fontSize: 10,
        textTransform: "uppercase",
        letterSpacing: "var(--tracking-wide)",
        flex: "none",
      }}
    >
      <span
        style={{
          width: 6,
          height: 6,
          borderRadius: "50%",
          background: color,
          animation: pulse ? "pulse 1.4s ease-in-out infinite" : "none",
        }}
      />
      {LABEL[state]}
    </span>
  );
}
