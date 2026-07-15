import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { NewSessionResponse, QuestionSpec } from "../types";
import type { Tokens, ThemeName } from "../theme";

export function getTheme(): Promise<Tokens> {
  return invoke("get_theme");
}

export function listThemes(): Promise<ThemeName[]> {
  return invoke("list_themes");
}

export function setTheme(name: string): Promise<Tokens> {
  return invoke("set_theme", { name });
}

export function startSession(tabId: string, cwd: string, yolo: boolean): Promise<NewSessionResponse> {
  return invoke("start_session", { tabId, cwd, yolo });
}

export function loadSession(
  tabId: string,
  sessionId: string,
  cwd: string,
): Promise<NewSessionResponse> {
  return invoke("load_session", { tabId, sessionId, cwd });
}

export function listSessions(cwd?: string): Promise<{ sessions: unknown[] }> {
  return invoke("list_sessions", { cwd });
}

export function sendPrompt(tabId: string, sessionId: string, text: string): Promise<void> {
  return invoke("send_prompt", { tabId, sessionId, text });
}

export function setMode(tabId: string, sessionId: string, modeId: string): Promise<unknown> {
  return invoke("set_mode", { tabId, sessionId, modeId });
}

export function setConfigOption(
  tabId: string,
  sessionId: string,
  configId: string,
  value: string,
): Promise<unknown> {
  return invoke("set_config_option", { tabId, sessionId, configId, value });
}

export function resolvePermission(
  tabId: string,
  requestId: unknown,
  optionId: string | null,
): Promise<void> {
  return invoke("resolve_permission", { tabId, requestId, optionId });
}

export function resolveQuestion(
  tabId: string,
  requestId: unknown,
  result: { dismissed: boolean; answers: string[][] },
): Promise<void> {
  return invoke("resolve_question", { tabId, requestId, result });
}

export function cancelPrompt(tabId: string, sessionId: string): Promise<void> {
  return invoke("cancel_prompt", { tabId, sessionId });
}

export function closeTab(tabId: string): Promise<void> {
  return invoke("close_tab", { tabId });
}

export interface SessionUpdateEventPayload {
  tabId: string;
  update: Record<string, unknown>;
}
export interface PermissionRequestEventPayload {
  tabId: string;
  requestId: unknown;
  params: { sessionId: string; toolCall: { toolCallId: string; title: string }; options: unknown[] };
}
export interface QuestionRequestEventPayload {
  tabId: string;
  requestId: unknown;
  params: { sessionId: string; requestId: string; questions: QuestionSpec[] };
}
export interface PromptDoneEventPayload {
  tabId: string;
  sessionId: string;
  ok: boolean;
  response?: { stopReason?: string };
  error?: string;
}
export interface ClosedEventPayload {
  tabId: string;
}

export function onSessionUpdate(cb: (p: SessionUpdateEventPayload) => void): Promise<UnlistenFn> {
  return listen<SessionUpdateEventPayload>("acp://session-update", (e) => cb(e.payload));
}
export function onPermissionRequest(
  cb: (p: PermissionRequestEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<PermissionRequestEventPayload>("acp://permission-request", (e) => cb(e.payload));
}
export function onQuestion(cb: (p: QuestionRequestEventPayload) => void): Promise<UnlistenFn> {
  return listen<QuestionRequestEventPayload>("acp://question", (e) => cb(e.payload));
}
export function onPromptDone(cb: (p: PromptDoneEventPayload) => void): Promise<UnlistenFn> {
  return listen<PromptDoneEventPayload>("acp://prompt-done", (e) => cb(e.payload));
}
export function onClosed(cb: (p: ClosedEventPayload) => void): Promise<UnlistenFn> {
  return listen<ClosedEventPayload>("acp://closed", (e) => cb(e.payload));
}
