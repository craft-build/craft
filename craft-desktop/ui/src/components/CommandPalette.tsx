import { useEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import type { Tokens } from "../theme";
import type { CommandDescriptor, CustomCommandDescriptor, ListCommandsResponse } from "../lib/acp";

export interface PaletteItem {
  name: string;
  description: string;
  acceptsArgs: boolean;
  strategy: CommandDescriptor["strategy"];
  /** Custom command display name (`/project:foo`) when this is a custom command. */
  customName?: string;
  /** Server-provided `_craft/*` method (for `craft_request` commands). */
  method?: string;
  /** Server-provided `kind` for `_craft/meta/prompt` commands. */
  metaKind?: string;
}

export type PaletteKeyHandler = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => void;

function itemsFrom(resp: ListCommandsResponse | null): PaletteItem[] {
  if (!resp) return [];
  const builtins: PaletteItem[] = resp.commands.map((c) => ({
    name: c.name,
    description: c.description,
    acceptsArgs: c.maxArgs > 0,
    strategy: c.strategy,
    method: c.method,
    metaKind: c.metaKind,
  }));
  const custom: PaletteItem[] = resp.custom.map((c: CustomCommandDescriptor) => ({
    name: c.displayName,
    description: c.description,
    acceptsArgs: c.acceptsArgs,
    strategy: "craft_request",
    customName: c.displayName,
  }));
  return [...builtins, ...custom];
}

/** Subsequence matcher: returns match indices if `pattern` is a subsequence
 * of `text` (case-insensitive), else null. Used for highlight rendering. */
function subseq(text: string, pattern: string): number[] | null {
  if (!pattern) return [];
  const t = text.toLowerCase();
  const p = pattern.toLowerCase();
  const indices: number[] = [];
  let pi = 0;
  for (let ti = 0; ti < t.length && pi < p.length; ti++) {
    if (t[ti] === p[pi]) {
      indices.push(ti);
      pi++;
    }
  }
  return pi === p.length ? indices : null;
}

interface Props {
  composerText: string;
  commands: ListCommandsResponse | null;
  t: Tokens;
  onRoute: (item: PaletteItem, args: string) => void;
  onComplete: (text: string) => void;
  onClose: () => void;
  /** Populated with a key handler the parent textarea delegates to while the
   * palette is open. Avoids moving focus away from the composer. */
  keyHandlerRef: React.MutableRefObject<PaletteKeyHandler | null>;
}

export function CommandPalette({
  composerText,
  commands,
  t,
  onRoute,
  onComplete,
  onClose,
  keyHandlerRef,
}: Props) {
  const [selected, setSelected] = useState(0);

  const stripped = composerText.startsWith("/") ? composerText.slice(1) : "";
  const firstSpace = stripped.search(/\s/);
  const cmdWord = firstSpace >= 0 ? stripped.slice(0, firstSpace) : stripped;
  const args = firstSpace >= 0 ? stripped.slice(firstSpace + 1) : "";

  const items = useMemo(() => itemsFrom(commands), [commands]);
  const matches = useMemo(() => {
    if (!cmdWord) return items.slice(0, 8);
    return items
      .map((it) => ({ it, indices: subseq(it.name, cmdWord) }))
      .filter((m): m is { it: PaletteItem; indices: number[] } => m.indices !== null)
      .sort((a, b) => a.indices.length - b.indices.length || a.it.name.localeCompare(b.it.name))
      .slice(0, 8)
      .map((m) => m.it);
  }, [items, cmdWord]);

  const activeIdx = Math.min(selected, Math.max(0, matches.length - 1));
  const matchesRef = useRef(matches);
  matchesRef.current = matches;
  const activeIdxRef = useRef(activeIdx);
  activeIdxRef.current = activeIdx;
  const argsRef = useRef(args);
  argsRef.current = args;

  useEffect(() => {
    keyHandlerRef.current = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
      const ms = matchesRef.current;
      if (ms.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelected((s) => (s + 1) % ms.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelected((s) => (s - 1 + ms.length) % ms.length);
      } else if (e.key === "Enter") {
        e.preventDefault();
        const item = ms[activeIdxRef.current];
        if (item) onRoute(item, argsRef.current);
      } else if (e.key === "Tab") {
        e.preventDefault();
        const item = ms[activeIdxRef.current];
        if (item) {
          const text = item.acceptsArgs ? `${item.name} ` : item.name;
          onComplete(text);
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    return () => {
      keyHandlerRef.current = null;
    };
  }, [keyHandlerRef, onRoute, onComplete, onClose]);

  if (matches.length === 0) return null;

  return (
    <div
      style={{
        position: "absolute",
        left: 20,
        bottom: 78,
        right: 20,
        maxWidth: 420,
        background: t.bgInset,
        border: `1px solid ${t.border}`,
        boxShadow: "0 8px 24px rgba(0,0,0,0.4)",
        zIndex: 50,
        fontFamily: "'JetBrains Mono', monospace",
        fontSize: 12,
      }}
    >
      {matches.map((item, idx) => (
        <div
          key={item.name}
          onMouseEnter={() => setSelected(idx)}
          onClick={() => onRoute(item, args)}
          style={{
            padding: "7px 11px",
            display: "flex",
            flexDirection: "column",
            gap: 1,
            cursor: "pointer",
            background: idx === activeIdx ? t.accentDim2 : "transparent",
            borderLeft:
              idx === activeIdx ? `2px solid ${t.accent}` : "2px solid transparent",
          }}
        >
          <div style={{ display: "flex", gap: 8, alignItems: "baseline" }}>
            <span style={{ color: idx === activeIdx ? t.accent : t.text, fontWeight: 600 }}>
              {item.name}
            </span>
            <span style={{ fontSize: 9, color: t.textFaint, textTransform: "uppercase" }}>
              {item.strategy.replace("_", " ")}
            </span>
          </div>
          {item.description && (
            <div style={{ fontSize: 11, color: t.textFaint }}>{item.description}</div>
          )}
        </div>
      ))}
    </div>
  );
}
