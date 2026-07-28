import type { Tokens } from "../theme";

type DiffLine = { type: "add" | "del" | "ctx"; text: string };

function lcsDiff(a: string[], b: string[]): DiffLine[] {
  const m = a.length;
  const n = b.length;
  const dp: number[][] = Array.from({ length: m + 1 }, () => new Array(n + 1).fill(0));
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const result: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < m && j < n) {
    if (a[i] === b[j]) {
      result.push({ type: "ctx", text: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      result.push({ type: "del", text: a[i] });
      i++;
    } else {
      result.push({ type: "add", text: b[j] });
      j++;
    }
  }
  while (i < m) result.push({ type: "del", text: a[i++] });
  while (j < n) result.push({ type: "add", text: b[j++] });
  return result;
}

export function DiffView({ oldText, newText, t }: { oldText: string; newText: string; t: Tokens }) {
  const oldLines = oldText ? oldText.split("\n") : [];
  const newLines = newText.split("\n");
  const lines = oldText ? lcsDiff(oldLines, newLines) : newLines.map((text) => ({ type: "add" as const, text }));

  return (
    <pre
      style={{
        fontFamily: "var(--font-mono)",
        fontSize: 11.5,
        lineHeight: 1.5,
        background: t.bgInset,
        border: `1px solid ${t.border}`,
        borderRadius: "var(--radius-md)",
        padding: "10px 12px",
        margin: "6px 0 0",
        overflow: "auto",
        whiteSpace: "pre",
      }}
      className="cd-scroll"
    >
      {lines.map((line, i) => {
        const isAdd = line.type === "add";
        const isDel = line.type === "del";
        const prefix = isAdd ? "+" : isDel ? "-" : " ";
        return (
          <div
            key={i}
            style={{
              color: isAdd ? t.success : isDel ? t.danger : t.textDim,
              background: isAdd
                ? `color-mix(in oklch, ${t.success} 12%, transparent)`
                : isDel
                  ? `color-mix(in oklch, ${t.danger} 12%, transparent)`
                  : "transparent",
              whiteSpace: "pre",
            }}
          >
            <span style={{ opacity: 0.6, marginRight: 4 }}>{prefix}</span>
            {line.text}
          </div>
        );
      })}
    </pre>
  );
}
