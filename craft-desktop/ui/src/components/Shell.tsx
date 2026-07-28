import { useEffect, useRef, useState } from "react";
import { brandGradient, modeColor, modeColorWash, modeLabel, type Tokens, type ThemeName } from "../theme";
import { useAppDispatch, useAppState, type AppState } from "../state/store";
import { closeTab, listSessions, listThemes, loadSession, setConfigOption, setMode, setTheme } from "../lib/acp";
import type { TabState } from "../types";
import { ChatPane } from "./ChatPane";
import { PlanGraph } from "./PlanGraph";
import { flattenTodos, todoMarker } from "../lib/todos";

const MODE_ORDER = ["build", "plan", "flow"];

export function Shell() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;
  const active = state.tabs.find((tab) => tab.tabId === state.activeTabId) ?? state.tabs[0];

  if (!active || !t) {
    return null;
  }

  const selectMode = async (m: string) => {
    if (!active.sessionId) return;
    await setMode(active.tabId, active.sessionId, m).catch(() => {});
    dispatch({ type: "SESSION_UPDATE", tabId: active.tabId, update: { sessionUpdate: "current_mode_update", currentModeId: m } });
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
          height: 44,
          flex: "none",
          display: "flex",
          alignItems: "center",
          padding: "0 16px 0 100px",
          background: t.bgElevated,
          borderBottom: `1px solid ${t.border}`,
          gap: 12,
          color: t.text,
        }}
      >
        <div
          data-tauri-drag-region
          style={{
            flex: 1,
            textAlign: "center",
            fontSize: 12.5,
            color: t.textFaint,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {active.title}
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 8, flex: "none", position: "relative" }}>
          <YoloToggle active={active} t={t} />
          <ModelPicker active={active} t={t} currentModelLabel={currentModelLabel} />
        </div>
      </div>

      <div style={{ flex: 1, display: "flex", minHeight: 0, overflow: "hidden" }}>
        <Sidebar
          active={active}
          t={t}
          onCloseTab={onCloseTab}
          selectMode={selectMode}
        />

        <div style={{ flex: 1, display: "flex", flexDirection: "column", minWidth: 0, overflow: "hidden", background: t.bg }}>
          <div style={{ display: "flex", gap: 2, padding: "10px 18px 0", flex: "none" }}>
            <div
              onClick={() => dispatch({ type: "SET_CENTER_TAB", tab: "chat" })}
              style={{
                padding: "5px 12px",
                fontSize: 10.5,
                letterSpacing: 0.3,
                cursor: "pointer",
                background: state.centerTab === "chat" ? t.bgInset : "transparent",
                color: state.centerTab === "chat" ? t.text : t.textFaint,
              }}
            >
              CHAT
            </div>
            <div
              onClick={() => dispatch({ type: "SET_CENTER_TAB", tab: "plan" })}
              style={{
                padding: "5px 12px",
                fontSize: 10.5,
                letterSpacing: 0.3,
                cursor: "pointer",
                background: state.centerTab === "plan" ? t.bgInset : "transparent",
                color: state.centerTab === "plan" ? t.text : t.textFaint,
              }}
            >
              PLAN GRAPH
            </div>
          </div>

          {state.centerTab === "chat" ? <ChatPane tab={active} t={t} /> : <PlanGraph tab={active} t={t} />}
        </div>
      </div>
    </div>
  );
}

function Sidebar({
  active,
  t,
  onCloseTab,
  selectMode,
}: {
  active: TabState;
  t: Tokens;
  onCloseTab: (tabId: string, e: React.MouseEvent) => void;
  selectMode: (m: string) => void;
}) {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const fetchedRef = useRef(false);

  useEffect(() => {
    if (state.historyOpen && !fetchedRef.current) {
      fetchedRef.current = true;
      listSessions(active.cwd, active.ssh)
        .then((resp) => {
          const sessions = (resp as unknown as { sessions?: unknown[] }).sessions ?? [];
          dispatch({ type: "HISTORY_LOADED", sessions: sessions as never });
        })
        .catch(() => {});
    }
    if (!state.historyOpen) fetchedRef.current = false;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.historyOpen]);

  const openHistorySession = async (row: AppState["historySessions"][number]) => {
    const tabId = crypto.randomUUID();
    const baseTitle = row.title?.trim() || row.sessionId.slice(0, 8);
    dispatch({
      type: "START_SESSION",
      tab: {
        tabId,
        sessionId: null,
        cwd: active.cwd,
        title: active.ssh ? `${active.ssh.host}:${baseTitle}` : baseTitle,
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
        ssh: active.ssh,
        commands: null,
      },
    });
    const resp = await loadSession(tabId, row.sessionId, active.cwd, active.ssh).catch(() => null);
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

  const flatTodos = flattenTodos(active.todos);

  return (
    <div
      style={{
        width: 260,
        flex: "none",
        display: "flex",
        flexDirection: "column",
        background: t.bgElevated,
        borderRight: `1px solid ${t.border}`,
        overflow: "hidden",
      }}
    >
      <div style={{ padding: "16px 16px 8px", flex: "none", display: "flex", alignItems: "center", justifyContent: "space-between", position: "relative" }}>
        <div style={{ fontSize: 10, letterSpacing: 0.5, color: t.textFaint }}>SESSIONS</div>
        <div style={{ display: "flex", gap: 2 }}>
          <SidebarIcon
            glyph="⚙"
            title="Settings"
            active={state.settingsOpen}
            t={t}
            onClick={() => dispatch({ type: "TOGGLE_SETTINGS" })}
          />
          <SidebarIcon
            glyph="⧖"
            title="Load previous session"
            active={state.historyOpen}
            t={t}
            onClick={() => dispatch({ type: "TOGGLE_HISTORY" })}
          />
        </div>

        {state.historyOpen && (
          <div
            style={{
              position: "absolute",
              top: 32,
              left: 16,
              width: 260,
              background: t.bgElevated,
              border: `1px solid ${t.border}`,
              borderRadius: "var(--radius-md)",
              boxShadow: "var(--shadow-lg)",
              padding: 6,
              zIndex: 40,
            }}
          >
            <div style={{ padding: "5px 8px 6px", fontSize: 9.5, color: t.textFaint, letterSpacing: 0.5 }}>PREVIOUS SESSIONS</div>
            {state.historySessions.length === 0 && (
              <div style={{ padding: 9, fontSize: 11.5, color: t.textFaint }}>No past sessions in this folder yet.</div>
            )}
            {state.historySessions.map((hs) => (
              <div
                key={hs.sessionId}
                onClick={() => openHistorySession(hs)}
                style={{ padding: "9px 9px", cursor: "pointer", display: "flex", flexDirection: "column", gap: 4 }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
                  <span style={{ width: 6, height: 6, borderRadius: "50%", background: t.textFaint, flex: "none" }} />
                  <span style={{ fontSize: 12, color: t.text, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                    {hs.title || hs.sessionId.slice(0, 12)}
                  </span>
                </div>
                <div style={{ fontSize: 10, color: t.textFaint, paddingLeft: 13 }}>{hs.updatedAt ?? ""}</div>
              </div>
            ))}
          </div>
        )}

        {state.settingsOpen && (
          <div
            style={{
              position: "absolute",
              top: 32,
              left: 16,
              width: 240,
              background: t.bgElevated,
              border: `1px solid ${t.border}`,
              borderRadius: "var(--radius-md)",
              boxShadow: "var(--shadow-lg)",
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

      <div style={{ flex: "none", display: "flex", flexDirection: "column", gap: 6, padding: "0 10px 10px" }}>
        {state.tabs.map((tab) => (
          <div
            key={tab.tabId}
            onClick={() => dispatch({ type: "SELECT_TAB", tabId: tab.tabId })}
            style={{
              padding: "11px 12px",
              background: tab.tabId === active.tabId ? t.bgInset : "transparent",
              border: `1px solid ${tab.tabId === active.tabId ? t.border : "transparent"}`,
              borderRadius: "var(--radius-sm)",
              cursor: "pointer",
              display: "flex",
              flexDirection: "column",
              gap: 6,
              position: "relative",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
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
                  fontSize: 12.5,
                  color: t.text,
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                  flex: 1,
                }}
              >
                {tab.pending ? `${tab.title} …` : tab.title}
              </span>
              <span
                onClick={(e) => onCloseTab(tab.tabId, e)}
                style={{ color: t.textFaint, fontSize: 12, padding: "0 2px", flex: "none" }}
              >
                &times;
              </span>
            </div>
            <div style={{ fontSize: 10.5, color: t.textFaint, paddingLeft: 14 }}>
              {tab.pending ? "running…" : `${modeLabel(tab.mode)} mode`}
            </div>
          </div>
        ))}
        <div
          onClick={() => dispatch({ type: "SET_SCREEN", screen: "onboarding" })}
          style={{ padding: "9px 12px", fontSize: 11.5, color: t.textFaint, cursor: "pointer" }}
        >
          + new session
        </div>
      </div>

      <div style={{ padding: "16px 16px 8px", flex: "none" }}>
        <div style={{ fontSize: 10, letterSpacing: 0.5, color: t.textFaint }}>TODO</div>
      </div>
      <div style={{ flex: 1, overflow: "auto", padding: "0 10px 10px", display: "flex", flexDirection: "column", gap: 2 }} className="cd-scroll">
        {flatTodos.length === 0 && (
          <div style={{ fontSize: 11.5, color: t.textFaint, padding: "3px 6px" }}>No todos yet.</div>
        )}
        {flatTodos.map((entry, i) => {
          const marker = todoMarker(entry.item.status, t);
          return (
            <div
              key={i}
              style={{
                display: "flex",
                alignItems: "flex-start",
                gap: 8,
                padding: "5px 6px",
                paddingLeft: 6 + entry.depth * 14,
              }}
            >
              <span style={{ fontSize: 11.5, color: marker.color, flex: "none", marginTop: 1 }}>{marker.mark}</span>
              <span
                style={{
                  fontSize: 12,
                  color: marker.textColor,
                  textDecoration: marker.strike ? "line-through" : "none",
                  lineHeight: 1.4,
                  wordBreak: "break-word",
                }}
              >
                {entry.item.content}
              </span>
            </div>
          );
        })}
      </div>

      <div style={{ padding: "17px 16px 14px", borderTop: `1px solid ${t.border}`, flex: "none", display: "flex", flexDirection: "column", gap: 8 }}>
        <div style={{ fontSize: 10, letterSpacing: 0.5, color: t.textFaint }}>AGENT</div>
        <div style={{ display: "flex" }}>
          {MODE_ORDER.map((m) => (
            <div
              key={m}
              onClick={() => selectMode(m)}
              style={{
                flex: 1,
                textAlign: "center",
                padding: "7px 0",
                fontSize: 11,
                cursor: "pointer",
                background: active.mode === m ? modeColorWash(m, t) : "transparent",
                color: active.mode === m ? modeColor(m, t) : t.textFaint,
                border: `1px solid ${active.mode === m ? modeColor(m, t) : t.border}`,
              }}
            >
              {modeLabel(m)}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function SidebarIcon({
  glyph,
  title,
  active,
  t,
  onClick,
}: {
  glyph: string;
  title: string;
  active: boolean;
  t: Tokens;
  onClick: () => void;
}) {
  return (
    <div
      onClick={onClick}
      title={title}
      style={{
        width: 20,
        height: 20,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        cursor: "pointer",
        borderRadius: "var(--radius-xs)",
        color: active ? t.accent : t.textDim,
        background: active ? t.accentDim : "transparent",
        fontSize: 12,
      }}
    >
      {glyph}
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

  // The backend can report the same model spec from more than one source
  // (live listing, static fallback, custom/dynamic provider config), so
  // dedupe by value here rather than trusting configOptions to be unique.
  const providers = new Map<string, { value: string; name: string }[]>();
  const seenValues = new Set<string>();
  for (const o of modelOption?.options ?? []) {
    if (seenValues.has(o.value)) continue;
    seenValues.add(o.value);
    const key = o.value.includes("/") ? o.value.split("/")[0] : "other";
    if (!providers.has(key)) providers.set(key, []);
    providers.get(key)!.push({ value: o.value, name: o.value.includes("/") ? o.value.split("/").slice(1).join("/") : o.value });
  }
  const providerKeys = [...providers.keys()];
  const currentProviderKey = modelOption?.currentValue?.includes("/")
    ? modelOption.currentValue.split("/")[0]
    : providerKeys[0];
  const tab = activeProvider && providers.has(activeProvider) ? activeProvider : currentProviderKey;
  // Provider tabs share the dropdown's width evenly, so widen the dropdown
  // (it's right-anchored, so it grows leftward) as more providers are
  // configured rather than letting the tab labels get crushed.
  const dropdownWidth = Math.min(720, Math.max(250, providerKeys.length * 76));

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
          padding: "5px 11px",
          background: t.bgInset,
          cursor: "pointer",
          border: `1px solid ${t.border}`,
          borderRadius: "var(--radius-sm)",
        }}
      >
        <span style={{ fontSize: 11 }}>
          model <span style={{ color: t.text, fontWeight: 600 }}>{currentModelLabel}</span>
        </span>
        <span style={{ color: t.textFaint, fontSize: 9 }}>&#9662;</span>
      </div>
      {state.modelPickerOpen && (
        <div
          style={{
            position: "absolute",
            top: 32,
            right: 0,
            width: dropdownWidth,
            background: t.bgElevated,
            border: `1px solid ${t.border}`,
            borderRadius: "var(--radius-md)",
            boxShadow: "var(--shadow-lg)",
            zIndex: 40,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          {providerKeys.length === 0 ? (
            <div style={{ padding: 12, fontSize: 11.5, color: t.textFaint }}>Fetching available models&hellip;</div>
          ) : (
            <>
              <div style={{ display: "flex", borderBottom: `1px solid ${t.border}` }}>
                {providerKeys.map((key, i) => (
                  <div
                    key={key}
                    onClick={() => setActiveProvider(key)}
                    style={{
                      flex: 1,
                      textAlign: "center",
                      padding: "8px 4px",
                      fontSize: 10,
                      letterSpacing: 0.3,
                      cursor: "pointer",
                      whiteSpace: "nowrap",
                      background: key === tab ? t.bgInset : "transparent",
                      color: key === tab ? t.text : t.textFaint,
                      borderRight: i < providerKeys.length - 1 ? `1px solid ${t.border}` : "none",
                      borderBottom: `2px solid ${key === tab ? t.accent : "transparent"}`,
                    }}
                  >
                    {providerDisplayName(key).toUpperCase()}
                  </div>
                ))}
              </div>
              <div style={{ padding: 6 }}>
                {(providers.get(tab) ?? []).map((m) => (
                  <div
                    key={m.value}
                    onClick={() => pick(m.value)}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      padding: "7px 8px",
                      cursor: "pointer",
                      background: m.value === modelOption?.currentValue ? t.accentDim : "transparent",
                    }}
                  >
                    <span style={{ fontSize: 11.5, color: t.text }}>{m.name}</span>
                    {m.value === modelOption?.currentValue && <span style={{ color: t.accent, fontSize: 11 }}>&#10003;</span>}
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
        padding: "5px 9px",
        fontSize: 11,
        fontWeight: 600,
        letterSpacing: 0.3,
        cursor: "pointer",
        background: isYolo ? t.warningDim : "transparent",
        color: isYolo ? t.warning : t.textFaint,
        border: `1px solid ${isYolo ? t.warning : t.border}`,
        borderRadius: "var(--radius-sm)",
      }}
    >
      <span style={{ fontSize: 11 }}>&#9889;</span>
      YOLO
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
        <div style={{ padding: 8, fontSize: 11, color: t.textFaint }}>Loading themes&hellip;</div>
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
                cursor: "pointer",
                fontSize: 12,
                color: t.text,
                background: selected ? t.accentDim : "transparent",
                borderRadius: "var(--radius-xs)",
                fontWeight: selected ? 600 : 400,
              }}
            >
              <span
                style={{
                  width: 12,
                  height: 12,
                  borderRadius: "50%",
                  flex: "none",
                  background: selected ? brandGradient(t) : "transparent",
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
