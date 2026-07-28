import { createContext, useContext, useEffect, useReducer, type Dispatch, type ReactNode } from "react";
import type {
  ChatItem,
  ContentBlock,
  PermissionOption,
  QuestionSpec,
  SessionUpdate,
  TabState,
  TodoItem,
  ToolCallEvent,
  ToolCallUpdateEvent,
} from "../types";
import type { Tokens } from "../theme";
import { getTheme, type ListCommandsResponse } from "../lib/acp";

export interface AppState {
  tokens: Tokens | null;
  themeName: string;
  tabs: TabState[];
  activeTabId: string | null;
  screen: "onboarding" | "main";
  centerTab: "chat" | "plan";
  historyOpen: boolean;
  modelPickerOpen: boolean;
  settingsOpen: boolean;
  historySessions: Array<{ sessionId: string; title?: string; cwd?: string; updatedAt?: string }>;
}

export type Action =
  | { type: "START_SESSION"; tab: TabState }
  | {
      type: "SESSION_META";
      tabId: string;
      sessionId?: string;
      modes?: TabState["modes"];
      configOptions?: TabState["configOptions"];
      title?: string;
    }
  | { type: "SESSION_UPDATE"; tabId: string; update: SessionUpdate }
  | { type: "TODO_UPDATE"; tabId: string; todos: TodoItem[] }
  | {
      type: "PERMISSION_REQUEST";
      tabId: string;
      requestId: unknown;
      title: string;
      options: PermissionOption[];
    }
  | { type: "PERMISSION_RESOLVED"; tabId: string; requestId: unknown; optionId: string | null }
  | {
      type: "QUESTION_REQUEST";
      tabId: string;
      requestId: unknown;
      questions: QuestionSpec[];
    }
  | { type: "QUESTION_RESOLVED"; tabId: string; requestId: unknown; answers: string[][] }
  | { type: "COMPOSER_TEXT"; tabId: string; text: string }
  | { type: "COMMANDS_LOADED"; tabId: string; commands: ListCommandsResponse }
  | { type: "CLEAR_ITEMS"; tabId: string }
  | { type: "PROMPT_SENT"; tabId: string; text: string }
  | { type: "PROMPT_DONE"; tabId: string }
  | { type: "CONNECTION_CLOSED"; tabId: string }
  | { type: "CONNECTION_ERROR"; tabId: string; message: string }
  | { type: "DISMISS_ERROR"; tabId: string }
  | { type: "TAB_CLOSED"; tabId: string }
  | { type: "SELECT_TAB"; tabId: string }
  | { type: "SET_SCREEN"; screen: AppState["screen"] }
  | { type: "SET_CENTER_TAB"; tab: AppState["centerTab"] }
  | { type: "TOGGLE_HISTORY" }
  | { type: "TOGGLE_MODEL_PICKER" }
  | { type: "TOGGLE_SETTINGS" }
  | { type: "THEME_LOADED"; tokens: Tokens }
  | { type: "SET_THEME"; tokens: Tokens }
  | { type: "HISTORY_LOADED"; sessions: AppState["historySessions"] };

export const initialState: AppState = {
  tokens: null,
  themeName: "craft",
  tabs: [],
  activeTabId: null,
  screen: "onboarding",
  centerTab: "chat",
  historyOpen: false,
  modelPickerOpen: false,
  settingsOpen: false,
  historySessions: [],
};

function textOf(block: ContentBlock | undefined): string {
  if (!block) return "";
  if (block.type === "text" && "text" in block) return (block as { text: string }).text;
  return "";
}

function updateTab(state: AppState, tabId: string, fn: (t: TabState) => TabState): AppState {
  return { ...state, tabs: state.tabs.map((t) => (t.tabId === tabId ? fn(t) : t)) };
}

function applySessionUpdate(tab: TabState, update: SessionUpdate): TabState {
  const items = tab.items.slice();
  const last = items[items.length - 1];

  switch (update.sessionUpdate) {
    case "user_message_chunk": {
      const text = textOf((update as { content: ContentBlock }).content);
      if (last?.kind === "user") {
        items[items.length - 1] = { ...last, text: last.text + text };
      } else {
        items.push({ kind: "user", id: crypto.randomUUID(), text });
      }
      return { ...tab, items };
    }
    case "agent_message_chunk": {
      const text = textOf((update as { content: ContentBlock }).content);
      if (last?.kind === "assistant-text") {
        items[items.length - 1] = { ...last, text: last.text + text };
      } else {
        items.push({ kind: "assistant-text", id: crypto.randomUUID(), text });
      }
      return { ...tab, items };
    }
    case "agent_thought_chunk": {
      const text = textOf((update as { content: ContentBlock }).content);
      if (last?.kind === "thinking") {
        items[items.length - 1] = { ...last, text: last.text + text };
      } else {
        items.push({ kind: "thinking", id: crypto.randomUUID(), text });
      }
      return { ...tab, items };
    }
    case "tool_call": {
      const u = update as ToolCallEvent;
      items.push({
        kind: "tool",
        id: u.toolCallId,
        title: u.title ?? u.toolCallId,
        toolKind: u.kind ?? "other",
        status: u.status ?? "pending",
        content: u.content ?? [],
        locations: u.locations ?? [],
      });
      return { ...tab, items };
    }
    case "tool_call_update": {
      const u = update as ToolCallUpdateEvent;
      const idx = items.findIndex((i) => i.kind === "tool" && i.id === u.toolCallId);
      if (idx >= 0) {
        const existing = items[idx] as Extract<ChatItem, { kind: "tool" }>;
        items[idx] = {
          ...existing,
          title: u.title ?? existing.title,
          status: u.status ?? existing.status,
          content: u.content ?? existing.content,
          locations: u.locations ?? existing.locations,
        };
      } else {
        items.push({
          kind: "tool",
          id: u.toolCallId,
          title: u.title ?? u.toolCallId,
          toolKind: u.kind ?? "other",
          status: u.status ?? "in_progress",
          content: u.content ?? [],
          locations: u.locations ?? [],
        });
      }
      return { ...tab, items };
    }
    case "plan": {
      const u = update as { entries: TabState["plan"] };
      return { ...tab, plan: u.entries };
    }
    case "current_mode_update": {
      const u = update as { currentModeId: string };
      return { ...tab, mode: u.currentModeId };
    }
    case "config_option_update": {
      const u = update as { configOptions: TabState["configOptions"] };
      return { ...tab, configOptions: u.configOptions };
    }
    case "session_info_update": {
      const u = update as { title?: string };
      return u.title ? { ...tab, title: u.title } : tab;
    }
    case "usage_update": {
      const u = update as { used: number; size: number };
      return { ...tab, contextUsed: u.used, contextSize: u.size };
    }
    default:
      return tab;
  }
}

function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "START_SESSION":
      return {
        ...state,
        tabs: [...state.tabs, action.tab],
        activeTabId: action.tab.tabId,
        screen: "main",
        centerTab: "chat",
      };
    case "SESSION_UPDATE":
      return updateTab(state, action.tabId, (t) => applySessionUpdate(t, action.update));
    case "TODO_UPDATE":
      return updateTab(state, action.tabId, (t) => ({ ...t, todos: action.todos }));
    case "SESSION_META":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        sessionId: action.sessionId ?? t.sessionId,
        modes: action.modes ?? t.modes,
        configOptions: action.configOptions ?? t.configOptions,
        title: action.title ?? t.title,
      }));
    case "PERMISSION_REQUEST":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        items: [
          ...t.items,
          {
            kind: "permission",
            id: crypto.randomUUID(),
            requestId: action.requestId,
            title: action.title,
            options: action.options,
            resolved: false,
          },
        ],
      }));
    case "PERMISSION_RESOLVED":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        items: t.items.map((i) =>
          i.kind === "permission" && i.requestId === action.requestId
            ? { ...i, resolved: true, chosenOptionId: action.optionId ?? undefined }
            : i,
        ),
      }));
    case "QUESTION_REQUEST":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        items: [
          ...t.items,
          {
            kind: "question",
            id: crypto.randomUUID(),
            requestId: action.requestId,
            questions: action.questions,
            resolved: false,
            answers: action.questions.map(() => []),
          },
        ],
      }));
    case "QUESTION_RESOLVED":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        items: t.items.map((i) =>
          i.kind === "question" && i.requestId === action.requestId
            ? { ...i, resolved: true, answers: action.answers }
            : i,
        ),
      }));
    case "COMPOSER_TEXT":
      return updateTab(state, action.tabId, (t) => ({ ...t, composerText: action.text }));
    case "COMMANDS_LOADED":
      return updateTab(state, action.tabId, (t) => ({ ...t, commands: action.commands }));
    case "CLEAR_ITEMS":
      return updateTab(state, action.tabId, (t) => ({ ...t, items: [] }));
    case "PROMPT_SENT":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        pending: true,
        composerText: "",
      }));
    case "PROMPT_DONE":
      return updateTab(state, action.tabId, (t) => ({ ...t, pending: false }));
    case "CONNECTION_CLOSED":
      return updateTab(state, action.tabId, (t) => ({
        ...t,
        pending: false,
        connectionError: t.connectionError ?? "Session ended unexpectedly",
      }));
    case "CONNECTION_ERROR":
      return updateTab(state, action.tabId, (t) => ({ ...t, pending: false, connectionError: action.message }));
    case "DISMISS_ERROR":
      return updateTab(state, action.tabId, (t) => ({ ...t, connectionError: null }));
    case "TAB_CLOSED": {
      const remaining = state.tabs.filter((t) => t.tabId !== action.tabId);
      const active =
        state.activeTabId === action.tabId ? (remaining[0]?.tabId ?? null) : state.activeTabId;
      return {
        ...state,
        tabs: remaining,
        activeTabId: active,
        screen: remaining.length === 0 ? "onboarding" : state.screen,
      };
    }
    case "SELECT_TAB":
      return { ...state, activeTabId: action.tabId, historyOpen: false };
    case "SET_SCREEN":
      return { ...state, screen: action.screen };
    case "SET_CENTER_TAB":
      return { ...state, centerTab: action.tab };
    case "TOGGLE_HISTORY":
      return {
        ...state,
        historyOpen: !state.historyOpen,
        modelPickerOpen: false,
        settingsOpen: false,
      };
    case "TOGGLE_MODEL_PICKER":
      return {
        ...state,
        modelPickerOpen: !state.modelPickerOpen,
        historyOpen: false,
        settingsOpen: false,
      };
    case "TOGGLE_SETTINGS":
      return {
        ...state,
        settingsOpen: !state.settingsOpen,
        historyOpen: false,
        modelPickerOpen: false,
      };
    case "THEME_LOADED":
      return { ...state, tokens: action.tokens, themeName: action.tokens.name };
    case "SET_THEME":
      return { ...state, tokens: action.tokens, themeName: action.tokens.name };
    case "HISTORY_LOADED":
      return { ...state, historySessions: action.sessions };
    default:
      return state;
  }
}

const StateCtx = createContext<AppState>(initialState);
const DispatchCtx = createContext<Dispatch<Action>>(() => {});

export function StoreProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, undefined, () => initialState);

  useEffect(() => {
    getTheme()
      .then((tokens) => dispatch({ type: "THEME_LOADED", tokens }))
      .catch(() => {});
  }, []);

  return (
    <StateCtx.Provider value={state}>
      <DispatchCtx.Provider value={dispatch}>{children}</DispatchCtx.Provider>
    </StateCtx.Provider>
  );
}

export function useAppState() {
  return useContext(StateCtx);
}
export function useAppDispatch() {
  return useContext(DispatchCtx);
}
