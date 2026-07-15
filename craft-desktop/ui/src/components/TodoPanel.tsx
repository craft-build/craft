import type { Tokens } from "../theme";
import type { TabState, TodoItem, TodoStatus } from "../types";

const STATUS_MARKER: Record<TodoStatus, string> = {
  pending: "[ ]",
  in_progress: "[\u2022]",
  completed: "[\u2713]",
  cancelled: "[x]",
};

function statusColor(status: TodoStatus, t: Tokens): string {
  switch (status) {
    case "completed":
    case "cancelled":
      return t.textFaint;
    case "in_progress":
      return t.accent;
    default:
      return t.textDim;
  }
}

function strikeThrough(status: TodoStatus): boolean {
  return status === "completed" || status === "cancelled";
}

interface FlatEntry {
  depth: number;
  item: TodoItem;
}

function flattenTree(items: TodoItem[]): FlatEntry[] {
  const idSet = new Set(items.map((it) => it.id).filter(Boolean));
  const visited = new Set<number>();
  const out: FlatEntry[] = [];

  const visit = (parentId: string | null, depth: number) => {
    items.forEach((item, i) => {
      if (visited.has(i)) return;
      const belongs =
        parentId === null
          ? !item.parent || !idSet.has(item.parent)
          : item.parent === parentId;
      if (!belongs) return;
      visited.add(i);
      out.push({ depth, item });
      if (item.id) visit(item.id, depth + 1);
    });
  };

  visit(null, 0);
  items.forEach((item, i) => {
    if (!visited.has(i)) out.push({ depth: 0, item });
  });
  return out;
}

export function TodoPanel({ tab, t }: { tab: TabState; t: Tokens }) {
  const flat = flattenTree(tab.todos);
  const done = tab.todos.filter(
    (it) => it.status === "completed" || it.status === "cancelled",
  ).length;

  return (
    <div
      style={{
        width: 280,
        flex: "none",
        display: "flex",
        flexDirection: "column",
        background: t.bgInset,
        borderLeft: `1px solid ${t.border}`,
        overflow: "hidden",
      }}
    >
      <div
        style={{
          padding: "12px 14px 10px",
          fontSize: 10,
          letterSpacing: 0.4,
          color: t.textFaint,
          borderBottom: `1px solid ${t.border}`,
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
        }}
      >
        <span>TODOS</span>
        <span>
          {done}/{tab.todos.length}
        </span>
      </div>
      <div style={{ flex: 1, overflow: "auto", padding: "8px 0" }} className="cd-scroll">
        {flat.map((entry, i) => {
          const { depth, item } = entry;
          const marker = STATUS_MARKER[item.status] ?? STATUS_MARKER.pending;
          const color = statusColor(item.status, t);
          return (
            <div
              key={i}
              style={{
                display: "flex",
                gap: 6,
                padding: "3px 14px",
                fontSize: 12,
                lineHeight: 1.5,
                color,
                paddingLeft: 14 + depth * 16,
              }}
            >
              <span style={{ flex: "none", color }}>{marker}</span>
              <span
                style={{
                  textDecoration: strikeThrough(item.status) ? "line-through" : "none",
                  wordBreak: "break-word",
                }}
              >
                {item.id && <span style={{ opacity: 0.7 }}>{item.id} </span>}
                {item.content}
                {item.owner && (
                  <span style={{ opacity: 0.5 }}> (@{item.owner})</span>
                )}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
