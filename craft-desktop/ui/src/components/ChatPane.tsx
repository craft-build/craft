import { useEffect, useMemo, useRef, useState } from "react";
import type { Tokens } from "../theme";
import { useAppDispatch } from "../state/store";
import { cancelPrompt, resolvePermission, resolveQuestion, sendPrompt, setMode } from "../lib/acp";
import type { ChatItem, ContentBlock, QuestionSpec, TabState, ToolCallContent } from "../types";
import { Markdown, stripAnchors, langFromPath } from "./Markdown";
import { DiffView } from "./DiffView";

const TOOL_ICON: Record<string, string> = {
  read: "◎",
  edit: "✎",
  execute: "❱",
  search: "◎",
  fetch: "◎",
  delete: "✗",
  move: "→",
  think: "•",
  switch_mode: "⇄",
  other: "●",
};

const STATUS_GLYPH: Record<string, string> = {
  pending: "…",
  in_progress: "…",
  completed: "✓",
  failed: "✗",
};

const COMPOSER_MODES = ["build", "plan", "flow"];

export function ChatPane({ tab, t }: { tab: TabState; t: Tokens }) {
  const dispatch = useAppDispatch();
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [tab.items.length]);

  const send = async () => {
    const text = tab.composerText.trim();
    if (!text || tab.pending || !tab.sessionId) return;
    dispatch({ type: "PROMPT_SENT", tabId: tab.tabId, text });
    await sendPrompt(tab.tabId, tab.sessionId, text).catch((e: unknown) => {
      dispatch({ type: "CONNECTION_ERROR", tabId: tab.tabId, message: String(e) });
    });
  };

  const stop = async () => {
    if (!tab.sessionId) return;
    await cancelPrompt(tab.tabId, tab.sessionId).catch((e: unknown) => {
      dispatch({ type: "CONNECTION_ERROR", tabId: tab.tabId, message: String(e) });
    });
  };

  const switchMode = async (mode: string) => {
    if (!tab.sessionId) return;
    await setMode(tab.tabId, tab.sessionId, mode).catch(() => {});
    dispatch({ type: "SESSION_UPDATE", tabId: tab.tabId, update: { sessionUpdate: "current_mode_update", currentModeId: mode } });
  };

  return (
    <>
      {tab.connectionError && (
        <div
          style={{
            flex: "none",
            margin: "12px 20px 0",
            padding: "9px 12px",
            borderRadius: 8,
            background: t.warningDim,
            border: `1px solid ${t.warning}`,
            display: "flex",
            alignItems: "center",
            gap: 10,
          }}
        >
          <span style={{ fontSize: 12, color: t.warning }}>&#9888;</span>
          <span style={{ flex: 1, fontSize: 12, color: t.text }}>{tab.connectionError}</span>
          <span
            onClick={() => dispatch({ type: "DISMISS_ERROR", tabId: tab.tabId })}
            style={{ fontSize: 13, color: t.textFaint, cursor: "pointer", padding: "0 2px" }}
          >
            &times;
          </span>
        </div>
      )}
      <div ref={scrollRef} style={{ flex: 1, overflow: "auto", padding: "18px 20px", display: "flex", flexDirection: "column", gap: 14 }} className="cd-scroll">
        {tab.items.length === 0 && (
          <div style={{ fontSize: 12.5, color: t.textFaint, lineHeight: 1.6 }}>
            Ask craft to make a change in <span style={{ color: t.textDim }}>{tab.cwd}</span>.
          </div>
        )}
        {tab.items.map((item, idx) => (
          <ChatItemView key={item.id} item={item} t={t} tab={tab} streaming={tab.pending && idx === tab.items.length - 1} />
        ))}
      </div>

      <div style={{ flex: "none", padding: "12px 16px 16px", display: "flex", flexDirection: "column", gap: 8 }}>
        <div style={{ display: "flex", gap: 6 }}>
          {COMPOSER_MODES.map((m) => (
            <div
              key={m}
              onClick={() => switchMode(m)}
              style={{
                padding: "4px 10px",
                borderRadius: 6,
                fontSize: 10.5,
                cursor: "pointer",
                background: tab.mode === m ? t.accentDim : t.bgInset,
                color: tab.mode === m ? t.accent : t.textFaint,
              }}
            >
              {m[0].toUpperCase() + m.slice(1)}
            </div>
          ))}
        </div>
        <div style={{ display: "flex", alignItems: "flex-end", gap: 8, padding: "10px 12px", borderRadius: 10, background: t.bgInset, border: `1px solid ${t.border}` }}>
          <textarea
            value={tab.composerText}
            onChange={(e) => dispatch({ type: "COMPOSER_TEXT", tabId: tab.tabId, text: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            placeholder="Ask craft to make a change…"
            rows={1}
            style={{
              flex: 1,
              resize: "none",
              background: "transparent",
              border: "none",
              outline: "none",
              color: t.text,
              fontFamily: "'JetBrains Mono', monospace",
              fontSize: 13,
              lineHeight: 1.5,
              maxHeight: 120,
            }}
          />
          <div
            onClick={tab.pending ? stop : send}
            style={{
              flex: "none",
              padding: "8px 14px",
              borderRadius: 7,
              fontSize: 11.5,
              fontWeight: 600,
              cursor: "pointer",
              background: tab.pending ? t.bgInset : t.accent,
              color: tab.pending ? t.textFaint : t.accentText,
            }}
          >
            {tab.pending ? "Stop" : "Send ↵"}
          </div>
        </div>
      </div>
    </>
  );
}

function ChatItemView({ item, t, tab, streaming }: { item: ChatItem; t: Tokens; tab: TabState; streaming?: boolean }) {
  if (item.kind === "user") {
    return (
      <div style={{ display: "flex", justifyContent: "flex-end" }}>
        <div style={{ maxWidth: "70%", padding: "11px 14px", borderRadius: 10, background: t.accentDim2, fontSize: 13, lineHeight: 1.55, color: t.text, whiteSpace: "pre-wrap" }}>
          {item.text}
        </div>
      </div>
    );
  }

  if (item.kind === "assistant-text") {
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 8, maxWidth: "78%" }}>
        <Markdown md={item.text} t={t} streaming={streaming} />
      </div>
    );
  }

  if (item.kind === "thinking") {
    return (
      <div style={{ fontSize: 12, color: t.textFaint, lineHeight: 1.5, animation: "pulse 1.4s ease-in-out infinite" }}>
        {item.text ? item.text : "craft is thinking…"}
      </div>
    );
  }

  if (item.kind === "tool") {
    return <ToolCallRow item={item} t={t} />;
  }

  if (item.kind === "permission") {
    return <PermissionCard item={item} t={t} tab={tab} />;
  }

  if (item.kind === "question") {
    return <QuestionCard item={item} t={t} tab={tab} />;
  }

  return null;
}

function ToolCallRow({
  item,
  t,
}: {
  item: Extract<ChatItem, { kind: "tool" }>;
  t: Tokens;
}) {
  const [expanded, setExpanded] = useState(false);

  const diff = item.content.find((c): c is Extract<ToolCallContent, { type: "diff" }> => c.type === "diff");
  const stats = diff ? diffStat(diff.oldText ?? "", diff.newText) : null;
  const text = stripAnchors(
    item.content
      .filter((c): c is Extract<ToolCallContent, { type: "content" }> => c.type === "content")
      .map((c) => textOf(c.content))
      .filter(Boolean)
      .join("\n"),
  );
  const hasDiff = !!diff;
  const hasText = text.length > 0;
  const hasOutput = hasDiff || hasText;

  const lang = langFromPath(item.locations[0]?.path ?? item.title);
  const mdText = useMemo(() => wrapCodeBlock(text, lang), [text, lang]);

  return (
    <div style={{ display: "flex", flexDirection: "column", maxWidth: "78%" }}>
      <div
        onClick={() => {
          if (hasOutput) setExpanded((v) => !v);
        }}
        style={{
          display: "flex",
          alignItems: "center",
          gap: 10,
          padding: "9px 12px",
          borderRadius: expanded ? "8px 8px 0 0" : 8,
          background: t.bgInset,
          border: `1px solid ${t.border}`,
          cursor: hasOutput ? "pointer" : "default",
        }}
      >
        <span style={{ fontSize: 12, color: t.accent, flex: "none" }}>{TOOL_ICON[item.toolKind] ?? "●"}</span>
        <span style={{ fontSize: 12, color: t.textDim, flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
          {item.title}
        </span>
        {stats && (
          <>
            <span style={{ fontSize: 11, color: t.success, flex: "none" }}>+{stats.add}</span>
            <span style={{ fontSize: 11, color: t.danger, flex: "none" }}>-{stats.del}</span>
          </>
        )}
        {hasOutput && <span style={{ fontSize: 10, color: t.textFaint, flex: "none" }}>{expanded ? "▾" : "▸"}</span>}
        <span
          style={{
            fontSize: 12,
            color: t.textFaint,
            flex: "none",
            animation: item.status === "in_progress" ? "pulse 1.4s ease-in-out infinite" : undefined,
          }}
        >
          {STATUS_GLYPH[item.status] ?? ""}
        </span>
      </div>
      {expanded && hasOutput && (
        <div
          style={{
            padding: "9px 12px",
            borderRadius: "0 0 8px 8px",
            background: t.bg,
            border: `1px solid ${t.border}`,
            borderTop: "none",
            maxHeight: 360,
            overflow: "auto",
          }}
          className="cd-scroll"
        >
          {diff && <DiffView oldText={diff.oldText ?? ""} newText={diff.newText} t={t} />}
          {hasText && <Markdown md={mdText} t={t} />}
        </div>
      )}
    </div>
  );
}

function textOf(block: ContentBlock | undefined): string {
  if (!block) return "";
  if (block.type === "text" && "text" in block) return (block as { text: string }).text;
  return "";
}

function PermissionCard({
  item,
  t,
  tab,
}: {
  item: Extract<ChatItem, { kind: "permission" }>;
  t: Tokens;
  tab: TabState;
}) {
  const dispatch = useAppDispatch();
  const [error, setError] = useState<string | null>(null);
  const allowOnce = item.options.find((o) => o.kind === "allow_once");
  const rejectOnce = item.options.find((o) => o.kind === "reject_once");
  const others = item.options.filter((o) => o !== allowOnce && o !== rejectOnce);

  const choose = async (optionId: string) => {
    setError(null);
    try {
      await resolvePermission(tab.tabId, item.requestId, optionId);
      dispatch({ type: "PERMISSION_RESOLVED", tabId: tab.tabId, requestId: item.requestId, optionId });
    } catch (e) {
      // Don't mark it resolved: the request may still be pending agent-side
      // if this call never made it through, so leave the buttons up to retry.
      setError(String(e));
    }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 10, maxWidth: "78%", padding: 14, borderRadius: 9, background: t.bgInset, border: `1px solid ${t.warning}` }}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span style={{ fontSize: 12, color: t.warning }}>&#9888;</span>
        <span style={{ fontSize: 12, color: t.text, fontWeight: 600 }}>Permission requested</span>
      </div>
      <div style={{ fontSize: 12.5, padding: "9px 11px", borderRadius: 6, background: t.bg, color: t.text, overflow: "auto", whiteSpace: "pre-wrap" }}>
        {item.title}
      </div>
      {!item.resolved ? (
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          {rejectOnce && (
            <div onClick={() => choose(rejectOnce.optionId)} style={{ padding: "8px 14px", borderRadius: 7, fontSize: 11.5, cursor: "pointer", background: t.bg, color: t.textDim, border: `1px solid ${t.border}` }}>
              Deny
            </div>
          )}
          {allowOnce && (
            <div onClick={() => choose(allowOnce.optionId)} style={{ padding: "8px 14px", borderRadius: 7, fontSize: 11.5, cursor: "pointer", background: t.accent, color: t.accentText, fontWeight: 600 }}>
              Allow once
            </div>
          )}
          {others.map((o) => (
            <div key={o.optionId} onClick={() => choose(o.optionId)} style={{ padding: "8px 14px", borderRadius: 7, fontSize: 11.5, cursor: "pointer", background: "transparent", color: t.textFaint, border: `1px solid ${t.border}` }}>
              {o.name}
            </div>
          ))}
        </div>
      ) : (
        <div style={{ fontSize: 11.5, color: t.textFaint }}>
          {item.chosenOptionId === allowOnce?.optionId ? "Allowed once." : "Resolved."}
        </div>
      )}
      {error && <div style={{ fontSize: 11, color: t.danger }}>Couldn't send response: {error}</div>}
    </div>
  );
}

function QuestionCard({
  item,
  t,
  tab,
}: {
  item: Extract<ChatItem, { kind: "question" }>;
  t: Tokens;
  tab: TabState;
}) {
  const dispatch = useAppDispatch();
  const [error, setError] = useState<string | null>(null);
  const [customText, setCustomText] = useState<Record<number, string>>({});
  const [showCustom, setShowCustom] = useState<Record<number, boolean>>({});
  const [selections, setSelections] = useState<string[][]>(() =>
    item.questions.map((_, i) => item.answers[i] ?? []),
  );

  const toggle = (qi: number, label: string) => {
    setSelections((prev) => {
      const cur = prev[qi] ?? [];
      if (item.questions[qi].multiSelect) {
        return prev.map((s, i) =>
          i === qi ? (cur.includes(label) ? cur.filter((x) => x !== label) : [...cur, label]) : s,
        );
      }
      return prev.map((s, i) => (i === qi ? (cur[0] === label ? [] : [label]) : s));
    });
  };

  const submit = async (dismissed: boolean) => {
    setError(null);
    const answers = dismissed
      ? item.questions.map(() => [])
      : selections.map((s, i) => {
          if (showCustom[i] && customText[i]?.trim()) return [customText[i].trim()];
          return s;
        });
    try {
      await resolveQuestion(tab.tabId, item.requestId, { dismissed, answers });
      dispatch({ type: "QUESTION_RESOLVED", tabId: tab.tabId, requestId: item.requestId, answers });
    } catch (e) {
      setError(String(e));
    }
  };

  const canSubmit = item.questions.every((_q, i) => {
    if (showCustom[i] && customText[i]?.trim()) return true;
    return (selections[i] ?? []).length > 0;
  });

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 12,
        maxWidth: "85%",
        padding: 14,
        borderRadius: 9,
        background: t.bgInset,
        border: `1px solid ${t.border}`,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span style={{ fontSize: 12, color: t.accent }}>?</span>
        <span style={{ fontSize: 12, color: t.text, fontWeight: 600 }}>
          {item.questions.length === 1 ? "Question" : `${item.questions.length} questions`}
        </span>
      </div>
      {item.questions.map((q: QuestionSpec, qi: number) => {
        const sel = selections[qi] ?? [];
        return (
          <div key={qi} style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <div style={{ fontSize: 12.5, color: t.text, fontWeight: 500 }}>{q.question}</div>
            {showCustom[qi] ? (
              <input
                value={customText[qi] ?? ""}
                onChange={(e) => setCustomText((p) => ({ ...p, [qi]: e.target.value }))}
                placeholder="Type your answer"
                style={{
                  fontSize: 12,
                  padding: "6px 9px",
                  borderRadius: 6,
                  border: `1px solid ${t.border}`,
                  background: t.bg,
                  color: t.text,
                  outline: "none",
                }}
              />
            ) : (
              q.options.length > 0 && (
                <div style={{ display: "flex", flexDirection: "column", gap: 3 }}>
                  {q.options.map((o) => {
                    const chosen = sel.includes(o.label);
                    return (
                      <div
                        key={o.label}
                        onClick={() => !item.resolved && toggle(qi, o.label)}
                        style={{
                          display: "flex",
                          alignItems: "flex-start",
                          gap: 7,
                          padding: "5px 8px",
                          borderRadius: 6,
                          cursor: item.resolved ? "default" : "pointer",
                          background: chosen ? t.accentDim2 : "transparent",
                        }}
                      >
                        <span style={{ fontSize: 11, color: chosen ? t.accent : t.textFaint }}>
                          {q.multiSelect ? (chosen ? "\u2611" : "\u2610") : chosen ? "\u25cf" : "\u25cb"}
                        </span>
                        <span style={{ display: "flex", flexDirection: "column" }}>
                          <span style={{ fontSize: 12, color: t.text }}>{o.label}</span>
                          {o.description && (
                            <span style={{ fontSize: 11, color: t.textFaint }}>{o.description}</span>
                          )}
                        </span>
                      </div>
                    );
                  })}
                </div>
              )
            )}
            {!item.resolved && (
              <div
                onClick={() => setShowCustom((p) => ({ ...p, [qi]: !p[qi] }))}
                style={{ fontSize: 11, color: t.textFaint, cursor: "pointer", marginTop: 2 }}
              >
                {showCustom[qi] ? "Choose from options" : "Type your own answer"}
              </div>
            )}
          </div>
        );
      })}
      {!item.resolved ? (
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          <div
            onClick={() => submit(false)}
            style={{
              padding: "8px 14px",
              borderRadius: 7,
              fontSize: 11.5,
              cursor: canSubmit ? "pointer" : "default",
              background: canSubmit ? t.accent : t.accentDim,
              color: t.accentText,
              fontWeight: 600,
              opacity: canSubmit ? 1 : 0.5,
            }}
          >
            Submit
          </div>
          <div
            onClick={() => submit(true)}
            style={{
              padding: "8px 14px",
              borderRadius: 7,
              fontSize: 11.5,
              cursor: "pointer",
              background: "transparent",
              color: t.textDim,
              border: `1px solid ${t.border}`,
            }}
          >
            Dismiss
          </div>
        </div>
      ) : (
        <div style={{ fontSize: 11.5, color: t.textFaint }}>
          {item.answers.every((a) => a.length === 0)
            ? "Dismissed."
            : "Answered."}
        </div>
      )}
      {error && <div style={{ fontSize: 11, color: t.danger }}>Couldn't send response: {error}</div>}
    </div>
  );
}

/**
 * Line-count delta for the +N/-N stat badge. Uses a multiset difference
 * (not a real LCS diff, which would be overkill for a badge) — lines that
 * appear in both texts the same number of times are treated as unchanged,
 * everything else counts as added or removed.
 */
function diffStat(oldText: string, newText: string): { add: number; del: number } {
  if (!oldText) return { add: newText ? newText.split("\n").length : 0, del: 0 };
  const counts = new Map<string, number>();
  for (const line of oldText.split("\n")) counts.set(line, (counts.get(line) ?? 0) + 1);
  let add = 0;
  for (const line of newText.split("\n")) {
    const remaining = counts.get(line) ?? 0;
    if (remaining > 0) counts.set(line, remaining - 1);
    else add++;
  }
  let del = 0;
  for (const remaining of counts.values()) del += Math.max(remaining, 0);
  return { add, del };
}

function wrapCodeBlock(text: string, lang: string | null): string {
  const trimmed = text.trim();
  if (!trimmed) return "";

  const lines = trimmed.split("\n").filter((l) => !/^\s*`{3,}\s*$/.test(l));
  const body = lines.join("\n").replace(/\n+$/, "");

  let maxRun = 0;
  for (const m of body.matchAll(/`+/g)) maxRun = Math.max(maxRun, m[0].length);
  const fence = "`".repeat(Math.max(3, maxRun + 1));

  return `${fence}${lang ?? ""}\n${body}\n${fence}`;
}
