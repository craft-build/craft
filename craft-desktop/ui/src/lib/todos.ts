import type { Tokens } from "../theme";
import type { TodoItem, TodoStatus } from "../types";

export interface FlatTodoEntry {
  depth: number;
  item: TodoItem;
}

export function flattenTodos(items: TodoItem[]): FlatTodoEntry[] {
  const idSet = new Set(items.map((it) => it.id).filter(Boolean));
  const visited = new Set<number>();
  const out: FlatTodoEntry[] = [];

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

export function todoMarker(status: TodoStatus, t: Tokens): { mark: string; color: string; textColor: string; strike: boolean } {
  switch (status) {
    case "completed":
      return { mark: "✓", color: t.success, textColor: t.textFaint, strike: true };
    case "cancelled":
      return { mark: "x", color: t.textFaint, textColor: t.textFaint, strike: true };
    case "in_progress":
      return { mark: "▸", color: t.accent, textColor: t.textDim, strike: false };
    default:
      return { mark: "○", color: t.textFaint, textColor: t.textDim, strike: false };
  }
}
