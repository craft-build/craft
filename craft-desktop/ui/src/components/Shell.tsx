import { useEffect, useRef, useState } from "react";
import { modeColor, modeLabel, type Tokens, type ThemeName } from "../theme";
import { useAppDispatch, useAppState, type AppState } from "../state/store";
import { closeTab, listSessions, listThemes, loadSession, setConfigOption, setMode, setTheme } from "../lib/acp";
import type { TabState } from "../types";
import { ChatPane } from "./ChatPane";
import { PlanGraph } from "./PlanGraph";
import { TodoPanel } from "./TodoPanel";

const MODE_ORDER = ["build", "plan", "flow"];

function formatTokens(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${Math.round(n / 1_000)}K`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

function ContextUsage({ used, size, t }: { used: number; size: number; t: Tokens }) {
  if (size <= 0) return null;
  const pct = Math.min(1, used / size);
  const barColor = pct > 0.95 ? t.danger : pct > 0.8 ? t.warning : t.accent;
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
      <div
        style={{
          width: 48,
          height: 4,
          borderRadius: 2,
          background: t.bgInset,
          overflow: "hidden",
        }}
      >
        <div style={{ width: `${pct * 100}%`, height: "100%", background: barColor }} />
      </div>
      <span style={{ fontSize: 11, color: t.textFaint, whiteSpace: "nowrap" }}>
        {formatTokens(used)} / {formatTokens(size)}
      </span>
    </div>
  );
}

export function Shell() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;
  const active = state.tabs.find((tab) => tab.tabId === state.activeTabId) ?? state.tabs[0];

  if (!active || !t) {
    return null;
  }

  const cycleMode = async () => {
    const idx = MODE_ORDER.indexOf(active.mode);
    const next = MODE_ORDER[(idx + 1) % MODE_ORDER.length];
    if (!active.sessionId) return;
    await setMode(active.tabId, active.sessionId, next).catch(() => {});
    dispatch({ type: "SESSION_UPDATE", tabId: active.tabId, update: { sessionUpdate: "current_mode_update", currentModeId: next } });
  };

  const onCloseTab = async (tabId: string, e: React.MouseEvent) => {
    e.stopPropagation();
    await closeTab(tabId).catch(() => {});
    dispatch({ type: "TAB_CLOSED", tabId });
  };

  const modelOption = active.configOptions.find((c) => c.id === "model");
  const currentModelLabel =
    modelOption?.options?.find((o) => o.value === modelOption.currentValue)?.name ??
    modelOption?.currentValue ??
    "Model";

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", background: t.bg, color: t.text }}>
      <div
        data-tauri-drag-region
        style={{
          height: 52,
          flex: "none",
          display: "flex",
          alignItems: "center",
          gap: 14,
          padding: "0 12px 0 100px",
          background: t.bgElevated,
          borderBottom: `1px solid ${t.border}`,
          color: t.text,
        }}
      >
        <div
          data-tauri-drag-region="deep"
          style={{ display: "flex", alignItems: "center", gap: 8, paddingRight: 10, borderRight: `1px solid ${t.border}` }}
        >
          <div
            style={{
              width: 22,
              height: 22,
              borderRadius: 6,
              background: t.accent,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              color: t.accentText,
              fontWeight: 700,
              fontSize: 12,
            }}
          >
            C
          </div>
          <div style={{ fontSize: 12.5, fontWeight: 600, color: t.text }}>craft desktop</div>
        </div>

        <div data-tauri-drag-region style={{ display: "flex", alignItems: "center", gap: 4, flex: 1, overflow: "auto" }} className="cd-scroll">
          {state.tabs.map((tab) => (
            <div
              key={tab.tabId}
              onClick={() => dispatch({ type: "SELECT_TAB", tabId: tab.tabId })}
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                padding: "7px 10px",
                borderRadius: 7,
                background: tab.tabId === active.tabId ? t.bgInset : "transparent",
                cursor: "pointer",
                maxWidth: 220,
                flex: "none",
              }}
            >
              <span
                style={{
                  width: 6,
                  height: 6,
                  borderRadius: "50%",
                  background: tab.tabId === active.tabId ? modeColor(tab.mode, t) : t.textFaint,
                  flex: "none",
                }}
              />
              <span
                style={{
                  fontSize: 12,
                  color: tab.tabId === active.tabId ? t.text : t.textDim,
                  whiteSpace: "nowrap",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                }}
              >
                {tab.pending ? `${tab.title} …` : tab.title}
              </span>
              <span onClick={(e) => onCloseTab(tab.tabId, e)} style={{ color: t.textFaint, fontSize: 12, padding: "0 2px" }}>
                &times;
              </span>
            </div>
          ))}
          <div
            onClick={() => dispatch({ type: "SET_SCREEN", screen: "onboarding" })}
            style={{
              width: 26,
              height: 26,
              flex: "none",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              borderRadius: 7,
              color: t.textFaint,
              cursor: "pointer",
              fontSize: 15,
            }}
          >
            +
          </div>
        </div>

        <div
          onClick={cycleMode}
          style={{
            padding: "6px 12px",
            borderRadius: 7,
            fontSize: 11.5,
            fontWeight: 600,
            letterSpacing: 0.3,
            cursor: "pointer",
            background: `color-mix(in oklch, ${modeColor(active.mode, t)} 16%, transparent)`,
            color: modeColor(active.mode, t),
            border: `1px solid color-mix(in oklch, ${modeColor(active.mode, t)} 40%, transparent)`,
          }}
        >
          {modeLabel(active.mode).toUpperCase()}
        </div>

        <YoloToggle active={active} t={t} />

        <ModelPicker active={active} t={t} currentModelLabel={currentModelLabel} />

        <ContextUsage used={active.contextUsed} size={active.contextSize} t={t} />

      </div>

      <div style={{ flex: 1, display: "flex", overflow: "hidden", position: "relative" }}>
        <LeftRail active={active} t={t} />

        <div style={{ flex: 1, display: "flex", flexDirection: "column", minWidth: 0, background: t.bg }}>
          <div style={{ display: "flex", gap: 4, padding: "10px 16px 0", flex: "none" }}>
            <div
              onClick={() => dispatch({ type: "SET_CENTER_TAB", tab: "chat" })}
              style={{
                padding: "6px 14px",
                borderRadius: 7,
                fontSize: 11.5,
                cursor: "pointer",
                background: state.centerTab === "chat" ? t.bgInset : "transparent",
                color: state.centerTab === "chat" ? t.text : t.textFaint,
                fontWeight: 500,
              }}
            >
              Chat
            </div>
            <div
              onClick={() => dispatch({ type: "SET_CENTER_TAB", tab: "plan" })}
              style={{
                padding: "6px 14px",
                borderRadius: 7,
                fontSize: 11.5,
                cursor: "pointer",
                background: state.centerTab === "plan" ? t.bgInset : "transparent",
                color: state.centerTab === "plan" ? t.text : t.textFaint,
                fontWeight: 500,
              }}
            >
              Plan graph
            </div>
          </div>

          {state.centerTab === "chat" ? <ChatPane tab={active} t={t} /> : <PlanGraph tab={active} t={t} />}
        </div>

        {active.todos.length > 0 && <TodoPanel tab={active} t={t} />}
      </div>
    </div>
  );
}

// Mirrors `ProviderKind::display_name()` in craft-providers/src/provider.rs.
// craft-acp doesn't hand us display names (`SessionConfigSelectOption.name`
// is just the raw "provider/model" spec), so we derive them the same way the
// TUI's model picker does — from the provider segment of the spec.
const PROVIDER_DISPLAY_NAMES: Record<string, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI",
  google: "Google",
  copilot: "Copilot",
  ollama: "Ollama",
  llamacpp: "LlamaCpp",
  mistral: "Mistral",
  deepseek: "DeepSeek",
  openrouter: "OpenRouter",
  synthetic: "Synthetic",
  tensorx: "TensorX",
  opencode: "Opencode",
  bedrock: "Bedrock",
};

function providerDisplayName(providerKey: string): string {
  return PROVIDER_DISPLAY_NAMES[providerKey] ?? providerKey[0].toUpperCase() + providerKey.slice(1);
}

function ModelPicker({
  active,
  t,
  currentModelLabel,
}: {
  active: TabState;
  t: Tokens;
  currentModelLabel: string;
}) {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const modelOption = active.configOptions.find((c) => c.id === "model");
  const [activeProvider, setActiveProvider] = useState<string | null>(null);

  const providers = new Map<string, { value: string; name: string }[]>();
  for (const o of modelOption?.options ?? []) {
    const key = o.value.includes("/") ? o.value.split("/")[0] : "other";
    if (!providers.has(key)) providers.set(key, []);
    providers.get(key)!.push({ value: o.value, name: o.value.includes("/") ? o.value.split("/").slice(1).join("/") : o.value });
  }
  const providerKeys = [...providers.keys()];
  const currentProviderKey = modelOption?.currentValue?.includes("/")
    ? modelOption.currentValue.split("/")[0]
    : providerKeys[0];
  const tab = activeProvider && providers.has(activeProvider) ? activeProvider : currentProviderKey;

  const pick = async (optionId: string) => {
    if (!active.sessionId) return;
    try {
      const resp = (await setConfigOption(active.tabId, active.sessionId, "model", optionId)) as {
        configOptions?: TabState["configOptions"];
      };
      if (resp?.configOptions) {
        dispatch({
          type: "SESSION_UPDATE",
          tabId: active.tabId,
          update: { sessionUpdate: "config_option_update", configOptions: resp.configOptions },
        });
      }
      dispatch({ type: "TOGGLE_MODEL_PICKER" });
    } catch (e) {
      console.error("failed to switch model", e);
    }
  };

  return (
    <div style={{ position: "relative" }}>
      <div
        onClick={() => dispatch({ type: "TOGGLE_MODEL_PICKER" })}
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          padding: "7px 12px",
          borderRadius: 7,
          background: t.bgInset,
          cursor: "pointer",
          border: `1px solid ${t.border}`,
        }}
      >
        <span style={{ width: 6, height: 6, borderRadius: "50%", background: t.accent }} />
        <span style={{ fontSize: 12, maxWidth: 160, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", color: t.text }}>
          {currentModelLabel}
        </span>
        <span style={{ color: t.textFaint, fontSize: 10 }}>&#9662;</span>
      </div>
      {state.modelPickerOpen && (
        <div
          style={{
            position: "absolute",
            top: 40,
            right: 0,
            width: 300,
            background: t.bgElevated,
            border: `1px solid ${t.border}`,
            borderRadius: 9,
            boxShadow: "0 12px 32px rgba(0,0,0,0.28)",
            zIndex: 40,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          {providerKeys.length === 0 ? (
            <div style={{ padding: 12, fontSize: 11.5, color: t.textFaint }}>Fetching available models…</div>
          ) : (
            <>
              <div
                style={{ display: "flex", gap: 2, padding: "6px 6px 0", overflowX: "auto", flex: "none" }}
                className="cd-scroll"
              >
                {providerKeys.map((key) => (
                  <div
                    key={key}
                    onClick={() => setActiveProvider(key)}
                    style={{
                      padding: "6px 10px",
                      borderRadius: "6px 6px 0 0",
                      fontSize: 10.5,
                      cursor: "pointer",
                      whiteSpace: "nowrap",
                      flex: "none",
                      background: key === tab ? t.bgInset : "transparent",
                      color: key === tab ? t.accent : t.textFaint,
                      fontWeight: key === tab ? 600 : 400,
                    }}
                  >
                    {providerDisplayName(key).toUpperCase()}
                  </div>
                ))}
              </div>
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 2,
                  padding: 6,
                  background: t.bgInset,
                  maxHeight: 280,
                  overflow: "auto",
                }}
                className="cd-scroll"
              >
                {(providers.get(tab) ?? []).map((m) => (
                  <div
                    key={m.value}
                    onClick={() => pick(m.value)}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      padding: "8px 8px",
                      borderRadius: 6,
                      cursor: "pointer",
                      background: m.value === modelOption?.currentValue ? t.accentDim : "transparent",
                    }}
                  >
                    <span style={{ fontSize: 12, color: t.text }}>{m.name}</span>
                    {m.value === modelOption?.currentValue && <span style={{ color: t.accent, fontSize: 12 }}>&#10003;</span>}
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

function YoloToggle({ active, t }: { active: TabState; t: Tokens }) {
  const dispatch = useAppDispatch();
  const yoloOption = active.configOptions.find((c) => c.id === "yolo");
  const isYolo = yoloOption?.currentValue === "true";

  const toggle = async () => {
    if (!active.sessionId) return;
    const next = isYolo ? "false" : "true";
    try {
      const resp = (await setConfigOption(active.tabId, active.sessionId, "yolo", next)) as {
        configOptions?: TabState["configOptions"];
      };
      if (resp?.configOptions) {
        dispatch({
          type: "SESSION_UPDATE",
          tabId: active.tabId,
          update: { sessionUpdate: "config_option_update", configOptions: resp.configOptions },
        });
      }
    } catch (e) {
      console.error("failed to toggle yolo", e);
    }
  };

  return (
    <div
      onClick={toggle}
      title={isYolo ? "Yolo on: permissions auto-approved" : "Enable yolo (auto-approve permissions)"}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 6,
        padding: "6px 10px",
        borderRadius: 7,
        fontSize: 11,
        fontWeight: 600,
        letterSpacing: 0.3,
        cursor: "pointer",
        background: isYolo ? `color-mix(in oklch, ${t.warning} 16%, transparent)` : "transparent",
        color: isYolo ? t.warning : t.textFaint,
        border: `1px solid ${isYolo ? `color-mix(in oklch, ${t.warning} 40%, transparent)` : t.border}`,
      }}
    >
      <span style={{ fontSize: 12 }}>{isYolo ? "\u26A1" : "\u26A1"}</span>
      YOLO
    </div>
  );
}

function LeftRail({ active, t }: { active: TabState; t: Tokens }) {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const fetchedRef = useRef(false);

  useEffect(() => {
    if (state.historyOpen && !fetchedRef.current) {
      fetchedRef.current = true;
      listSessions(active.cwd)
        .then((resp) => {
          const sessions = (resp as unknown as { sessions?: unknown[] }).sessions ?? [];
          dispatch({
            type: "HISTORY_LOADED",
            sessions: sessions as never,
          });
        })
        .catch(() => {});
    }
    if (!state.historyOpen) fetchedRef.current = false;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.historyOpen]);

  const openHistorySession = async (row: AppState["historySessions"][number]) => {
    const tabId = crypto.randomUUID();
    dispatch({
      type: "START_SESSION",
      tab: {
        tabId,
        sessionId: null,
        cwd: active.cwd,
        title: row.title?.trim() || row.sessionId.slice(0, 8),
        mode: "build",
        modes: [],
        configOptions: [],
        items: [],
        plan: [],
        todos: [],
        composerText: "",
        pending: false,
        connectionError: null,
        contextUsed: 0,
        contextSize: 0,
      },
    });
    const resp = await loadSession(tabId, row.sessionId, active.cwd).catch(() => null);
    if (!resp) {
      dispatch({ type: "TAB_CLOSED", tabId });
      return;
    }
    dispatch({
      type: "SESSION_META",
      tabId,
      sessionId: resp.sessionId,
      modes: resp.modes?.availableModes ?? [],
      configOptions: resp.configOptions ?? [],
    });
  };

  return (
    <div
      style={{
        width: 50,
        flex: "none",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        padding: "12px 0",
        gap: 6,
        background: t.bgInset,
        borderRight: `1px solid ${t.border}`,
      }}
    >
      <div
        onClick={() => dispatch({ type: "SET_SCREEN", screen: "onboarding" })}
        title="New session"
        style={{
          width: 32,
          height: 32,
          borderRadius: 8,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          cursor: "pointer",
          color: t.textDim,
          fontSize: 16,
        }}
      >
        +
      </div>

      <div style={{ position: "relative" }}>
        <div
          onClick={() => dispatch({ type: "TOGGLE_HISTORY" })}
          title="History"
          style={{
            width: 32,
            height: 32,
            borderRadius: 8,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            cursor: "pointer",
            color: state.historyOpen ? t.accent : t.textDim,
            background: state.historyOpen ? t.accentDim : "transparent",
            fontSize: 14,
          }}
        >
          &#8801;
        </div>
        {state.historyOpen && (
          <div
            style={{
              position: "absolute",
              top: 0,
              left: 44,
              width: 280,
              background: t.bgElevated,
              border: `1px solid ${t.border}`,
              borderRadius: 9,
              boxShadow: "0 12px 32px rgba(0,0,0,0.28)",
              padding: 8,
              zIndex: 40,
              display: "flex",
              flexDirection: "column",
              gap: 3,
              maxHeight: 360,
              overflow: "auto",
            }}
            className="cd-scroll"
          >
            <div style={{ padding: "4px 8px 6px", fontSize: 10, color: t.textFaint, letterSpacing: 0.4 }}>
              SESSION HISTORY &middot; {active.cwd}
            </div>
            {state.historySessions.length === 0 && (
              <div style={{ padding: 9, fontSize: 11.5, color: t.textFaint }}>No past sessions in this folder yet.</div>
            )}
            {state.historySessions.map((hs) => (
              <div
                key={hs.sessionId}
                onClick={() => openHistorySession(hs)}
                style={{ padding: "9px 9px", borderRadius: 7, cursor: "pointer", display: "flex", flexDirection: "column", gap: 3 }}
              >
                <div style={{ fontSize: 12, color: t.text }}>{hs.title || hs.sessionId.slice(0, 12)}</div>
                <div style={{ fontSize: 10.5, color: t.textFaint }}>{hs.updatedAt ?? ""}</div>
              </div>
            ))}
          </div>
        )}
      </div>

      <div style={{ flex: 1 }} />

      <div style={{ position: "relative" }}>
        <div
          onClick={() => dispatch({ type: "TOGGLE_SETTINGS" })}
          title="Settings"
          style={{
            width: 32,
            height: 32,
            borderRadius: 8,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            cursor: "pointer",
            color: state.settingsOpen ? t.accent : t.textDim,
            background: state.settingsOpen ? t.accentDim : "transparent",
            fontSize: 14,
          }}
        >
          &#9881;
        </div>
        {state.settingsOpen && (
          <div
            style={{
              position: "absolute",
              bottom: 0,
              left: 44,
              width: 240,
              background: t.bgElevated,
              border: `1px solid ${t.border}`,
              borderRadius: 9,
              boxShadow: "0 12px 32px rgba(0,0,0,0.28)",
              padding: 10,
              zIndex: 40,
              display: "flex",
              flexDirection: "column",
              gap: 10,
            }}
          >
            <div style={{ fontSize: 10, color: t.textFaint, letterSpacing: 0.4 }}>THEME</div>
            <ThemePicker currentName={state.themeName} />
          </div>
        )}
      </div>
    </div>
  );
}

function ThemePicker({ currentName }: { currentName: string }) {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;
  const [themes, setThemes] = useState<ThemeName[] | null>(null);

  useEffect(() => {
    listThemes()
      .then(setThemes)
      .catch(() => {});
  }, []);

  if (!t) return null;

  const pick = async (name: string) => {
    try {
      const tokens = await setTheme(name);
      dispatch({ type: "SET_THEME", tokens });
    } catch (e) {
      console.error("failed to switch theme", e);
    }
  };

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 2,
        maxHeight: 320,
        overflow: "auto",
      }}
      className="cd-scroll"
    >
      {themes === null ? (
        <div style={{ padding: 8, fontSize: 11, color: t.textFaint }}>Loading themes…</div>
      ) : (
        themes.map((theme) => {
          const selected = theme.name === currentName;
          return (
            <div
              key={theme.name}
              onClick={() => pick(theme.name)}
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                padding: "7px 9px",
                borderRadius: 7,
                cursor: "pointer",
                fontSize: 12,
                color: t.text,
                background: selected ? t.accentDim : "transparent",
                fontWeight: selected ? 600 : 400,
              }}
            >
              <span
                style={{
                  width: 12,
                  height: 12,
                  borderRadius: "50%",
                  flex: "none",
                  border: `1px solid ${t.border}`,
                }}
              />
              <span style={{ flex: 1 }}>{theme.label}</span>
              {selected && <span style={{ color: t.accent, fontSize: 12 }}>&#10003;</span>}
            </div>
          );
        })
      )}
    </div>
  );
}
