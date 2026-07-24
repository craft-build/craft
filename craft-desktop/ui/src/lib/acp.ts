import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { NewSessionResponse, QuestionSpec, SshTarget, TodoItem } from "../types";
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

export function startSession(
  tabId: string,
  cwd: string,
  yolo: boolean,
  ssh: SshTarget | null,
): Promise<NewSessionResponse> {
  return invoke("start_session", { tabId, cwd, yolo, ssh });
}

export function loadSession(
  tabId: string,
  sessionId: string,
  cwd: string,
  ssh: SshTarget | null,
): Promise<NewSessionResponse> {
  return invoke("load_session", { tabId, sessionId, cwd, ssh });
}

export function listSessions(cwd?: string, ssh?: SshTarget | null): Promise<{ sessions: unknown[] }> {
  return invoke("list_sessions", { cwd, ssh });
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

export function listCommands(tabId: string): Promise<ListCommandsResponse> {
  return invoke("list_commands", { tabId });
}

export function craftCommand(
  tabId: string,
  method: string,
  params: Record<string, unknown>,
): Promise<unknown> {
  return invoke("craft_command", { tabId, method, params });
}

/** Maps a `/name` builtin to its `_craft/*` method and dispatches. The method
 * (and, for `_craft/meta/prompt`, the `kind`) come from the server's
 * `_craft/listCommands` response, so this function does not keep its own
 * copy of the routing table. Custom commands (`/project:foo`) route through
 * `_craft/command/run`. */
export async function craftCommandRoute(
  tabId: string,
  sessionId: string,
  item: { name: string; customName?: string; method?: string; metaKind?: string },
  args: string,
): Promise<unknown> {
  if (item.customName) {
    return craftCommand(tabId, "_craft/command/run", {
      sessionId,
      name: item.customName,
      args,
    });
  }
  const method = item.method;
  if (!method) {
    throw new Error(`no _craft method for ${item.name}`);
  }
  const params: Record<string, unknown> = { sessionId };
  if (method === "_craft/meta/prompt") {
    if (!item.metaKind) {
      throw new Error(`meta command ${item.name} has no kind`);
    }
    params.kind = item.metaKind;
  } else if (item.name === "/cd") {
    params.cwd = args.trim();
  }
  return craftCommand(tabId, method, params);
}

export interface CommandDescriptor {
  name: string;
  description: string;
  maxArgs: number;
  strategy: "acp_standard" | "craft_request" | "passthrough" | "client";
  category: string;
  /** `_craft/*` method for `craft_request` commands; absent otherwise.
   * Server-provided so the client doesn't keep its own routing table. */
  method?: string;
  /** `kind` for `_craft/meta/prompt` commands; absent otherwise. */
  metaKind?: string;
}

export interface CustomCommandDescriptor {
  name: string;
  displayName: string;
  description: string;
  acceptsArgs: boolean;
  scope: "project" | "user";
}

export interface ListCommandsResponse {
  commands: CommandDescriptor[];
  custom: CustomCommandDescriptor[];
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

export interface TodoUpdateEventPayload {
  tabId: string;
  todos: TodoItem[];
}

export function onTodoUpdate(cb: (p: TodoUpdateEventPayload) => void): Promise<UnlistenFn> {
  return listen<TodoUpdateEventPayload>("acp://todo-update", (e) => cb(e.payload));
}
