// Wire-format types for the slice of the Agent Client Protocol craft-desktop
// consumes. Field names are camelCase to match the JSON craft-acp emits
// (see agent-client-protocol-schema's `#[serde(rename_all = "camelCase")]`).

import type { ListCommandsResponse } from "./lib/acp";

export type ToolCallStatus = "pending" | "in_progress" | "completed" | "failed";
export type ToolKind =
  | "read"
  | "edit"
  | "delete"
  | "move"
  | "search"
  | "execute"
  | "think"
  | "fetch"
  | "switch_mode"
  | "other";
export type PlanEntryStatus = "pending" | "in_progress" | "completed";
export type PlanEntryPriority = "high" | "medium" | "low";
export type TodoStatus = "pending" | "in_progress" | "completed" | "cancelled";

export interface TodoItem {
  id: string;
  content: string;
  status: TodoStatus;
  parent?: string;
  owner?: string;
}

export interface TextContentBlock {
  type: "text";
  text: string;
}
export type ContentBlock = TextContentBlock | { type: string; [k: string]: unknown };

export interface ContentChunk {
  content: ContentBlock;
}

export interface ToolCallLocation {
  path: string;
  line?: number;
}

export interface DiffContent {
  type: "diff";
  path: string;
  oldText?: string | null;
  newText: string;
}
export interface TextToolContent {
  type: "content";
  content: ContentBlock;
}
export type ToolCallContent = DiffContent | TextToolContent | { type: string; [k: string]: unknown };

export interface ToolCallEvent {
  sessionUpdate: "tool_call";
  toolCallId: string;
  title: string;
  kind?: ToolKind;
  status?: ToolCallStatus;
  content?: ToolCallContent[];
  locations?: ToolCallLocation[];
  rawInput?: unknown;
  rawOutput?: unknown;
}

export interface ToolCallUpdateEvent {
  sessionUpdate: "tool_call_update";
  toolCallId: string;
  title?: string;
  kind?: ToolKind;
  status?: ToolCallStatus;
  content?: ToolCallContent[];
  locations?: ToolCallLocation[];
  rawInput?: unknown;
  rawOutput?: unknown;
}

export interface PlanEntry {
  content: string;
  priority: PlanEntryPriority;
  status: PlanEntryStatus;
}
export interface PlanEvent {
  sessionUpdate: "plan";
  entries: PlanEntry[];
}

export interface SessionConfigSelectOption {
  value: string;
  name: string;
  description?: string | null;
}
export interface SessionConfigOption {
  id: string;
  name: string;
  description?: string | null;
  category?: string;
  type: "select";
  currentValue: string;
  options: SessionConfigSelectOption[];
}
export interface ConfigOptionUpdateEvent {
  sessionUpdate: "config_option_update";
  configOptions: SessionConfigOption[];
}

export interface CurrentModeUpdateEvent {
  sessionUpdate: "current_mode_update";
  currentModeId: string;
}

export interface SessionInfoUpdateEvent {
  sessionUpdate: "session_info_update";
  title?: string;
}

export interface MessageChunkEvent {
  sessionUpdate: "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk";
  content: ContentBlock;
}

export interface UsageUpdateEvent {
  sessionUpdate: "usage_update";
  used: number;
  size: number;
}

export type SessionUpdate =
  | MessageChunkEvent
  | ToolCallEvent
  | ToolCallUpdateEvent
  | PlanEvent
  | ConfigOptionUpdateEvent
  | CurrentModeUpdateEvent
  | SessionInfoUpdateEvent
  | UsageUpdateEvent
  | { sessionUpdate: string; [k: string]: unknown };

export interface PermissionOption {
  optionId: string;
  name: string;
  kind: "allow_once" | "allow_always" | "reject_once" | "reject_always";
}

export interface QuestionOption {
  label: string;
  description?: string | null;
}

export interface QuestionSpec {
  question: string;
  header?: string | null;
  options: QuestionOption[];
  multiSelect?: boolean;
}

export interface SessionModeOption {
  id: string;
  name: string;
}
export interface SessionModeState {
  currentModeId: string;
  availableModes: SessionModeOption[];
}

export interface NewSessionResponse {
  sessionId: string;
  modes?: SessionModeState;
  configOptions?: SessionConfigOption[];
}

// --- App-level chat item model, folded from the SessionUpdate stream ---

export type ChatItem =
  | { kind: "user"; id: string; text: string }
  | { kind: "assistant-text"; id: string; text: string }
  | { kind: "thinking"; id: string; text: string }
  | {
      kind: "tool";
      id: string;
      title: string;
      toolKind: ToolKind;
      status: ToolCallStatus;
      content: ToolCallContent[];
      locations: ToolCallLocation[];
    }
  | {
      kind: "permission";
      id: string;
      requestId: unknown;
      title: string;
      options: PermissionOption[];
      resolved: boolean;
      chosenOptionId?: string;
    }
  | {
      kind: "question";
      id: string;
      requestId: unknown;
      questions: QuestionSpec[];
      resolved: boolean;
      answers: string[][];
    };

export type CenterTab = "chat" | "plan";

export interface SessionSummary {
  id: string;
  cwd?: string;
  title?: string;
  updatedAt?: string;
}

export interface SshTarget {
  host: string;
  remoteCraft?: string;
}

export interface TabState {
  tabId: string;
  sessionId: string | null;
  cwd: string;
  title: string;
  mode: string;
  modes: SessionModeOption[];
  configOptions: SessionConfigOption[];
  items: ChatItem[];
  plan: PlanEntry[];
  todos: TodoItem[];
  composerText: string;
  pending: boolean;
  connectionError: string | null;
  contextUsed: number;
  contextSize: number;
  /** Null for a local session; set for SSH-launched tabs so history, prompt
   * routing, and titles reuse the same transport. */
  ssh: SshTarget | null;
  /** Cached `_craft/listCommands` response for the `/` palette. Null until
   * fetched (right after session start). */
  commands: ListCommandsResponse | null;
}
