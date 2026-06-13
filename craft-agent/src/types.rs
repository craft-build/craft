use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flume::Sender;
use craft_config::CompressionConfig;
use craft_providers::{AgentError, ContentBlock, Message, Role, StopReason, TokenUsage};

use crate::compression;
use craft_tool_macro::{ArgEnum, Args};
use serde::{Deserialize, Serialize};

pub const NO_FILES_FOUND: &str = "No files found";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub groups: Vec<GrepMatchGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchGroup {
    pub lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLine {
    pub line_nr: usize,
    pub text: String,
    pub is_match: bool,
}

impl GrepLine {
    pub fn matched(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: true,
        }
    }

    pub fn context(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: false,
        }
    }
}

impl GrepMatchGroup {
    pub fn single(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            lines: vec![GrepLine::matched(line_nr, text)],
        }
    }

    pub fn match_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_match).count()
    }
}

impl GrepFileEntry {
    pub fn match_count(&self) -> usize {
        self.groups.iter().map(|g| g.match_count()).sum()
    }
}

#[derive(Args, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskNode {
    #[param(description = "Hierarchical id: T1, T1.1, T1.1.2 (top-level starts at T1)")]
    #[serde(default)]
    pub id: String,
    #[param(description = "Parent task id; omit for top-level tasks")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[param(description = "Task description")]
    pub content: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(description = "Subagent name owning this task, if delegated")]
    pub owner: Option<String>,
}

impl TaskNode {
    pub fn is_valid_id(id: &str) -> bool {
        let Some(rest) = id.strip_prefix('T') else {
            return false;
        };
        !rest.is_empty()
            && rest.split('.').all(|comp| {
                !comp.is_empty()
                    && !comp.starts_with('0')
                    && comp.bytes().all(|b| b.is_ascii_digit())
            })
    }
}

pub fn flatten_task_tree(items: &[TaskNode]) -> Vec<(usize, &TaskNode)> {
    let id_exists = |id: &str| !id.is_empty() && items.iter().any(|t| t.id == id);
    let mut visited = vec![false; items.len()];
    let mut out = Vec::new();
    fn visit<'a>(
        items: &'a [TaskNode],
        parent_id: Option<&str>,
        id_exists: &impl Fn(&str) -> bool,
        depth: usize,
        visited: &mut [bool],
        out: &mut Vec<(usize, &'a TaskNode)>,
    ) {
        for (i, node) in items.iter().enumerate() {
            if visited[i] {
                continue;
            }
            let mine = match (&node.parent, parent_id) {
                (None, None) => true,
                (Some(np), Some(p)) => np == p,
                (Some(np), None) => !id_exists(np),
                (None, Some(_)) => false,
            };
            if !mine {
                continue;
            }
            visited[i] = true;
            out.push((depth, node));
            if !node.id.is_empty() {
                visit(items, Some(&node.id), id_exists, depth + 1, visited, out);
            }
        }
    }
    visit(items, None, &id_exists, 0, &mut visited, &mut out);
    for (i, node) in items.iter().enumerate() {
        if !visited[i] {
            out.push((0, node));
        }
    }
    out
}

#[derive(ArgEnum, Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Completed => "[✓]",
            Self::InProgress => "[•]",
            Self::Pending => "[ ]",
            Self::Cancelled => "[x]",
        }
    }
}

#[derive(ArgEnum, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, strum::Display)]
#[strum(serialize_all = "UPPERCASE")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub confidence: f64,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolInput {
    Code { language: String, code: String },
    Script { language: String, code: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BatchToolStatus {
    Pending,
    InProgress,
    Success,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchToolEntry {
    pub tool: String,
    pub summary: String,
    pub status: BatchToolStatus,
    pub input: Option<ToolInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<ToolOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionBlock {
    pub path: String,
    pub content: String,
}

fn append_instructions(out: &mut String, blocks: &[InstructionBlock]) {
    for block in blocks {
        out.push_str("\n\n---\nInstructions from: ");
        out.push_str(&block.path);
        out.push('\n');
        out.push_str(&block.content);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Plain(String),
    Markdown(String),
    ReadCode {
        path: String,
        start_line: usize,
        lines: Vec<String>,
        #[serde(default)]
        total_lines: usize,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
        #[serde(default)]
        no_compress: bool,
    },
    ReadDir {
        text: String,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    Diff {
        path: String,
        before: String,
        after: String,
        summary: String,
    },
    TodoList(Vec<TaskNode>),
    WriteCode {
        path: String,
        byte_count: usize,
        lines: Vec<String>,
    },

    GrepResult {
        entries: Vec<GrepFileEntry>,
    },
    Batch {
        entries: Vec<BatchToolEntry>,
        text: String,
        #[serde(default)]
        no_compress: bool,
    },
    Findings(Vec<Finding>),
    ReviewResult {
        findings: Vec<Finding>,
        verdict: String,
    },
    Instructions {
        blocks: Vec<InstructionBlock>,
    },
}

/// Saturating arithmetic so callers can't overflow with any combination of inputs.
fn lines_remaining_after(total: usize, start_line: usize, shown: usize) -> usize {
    let end = start_line.saturating_add(shown).saturating_sub(1);
    total.saturating_sub(end)
}

impl ToolOutput {
    pub fn written_path(&self) -> Option<&str> {
        match self {
            Self::WriteCode { path, .. } | Self::Diff { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn instructions(&self) -> Option<&[InstructionBlock]> {
        match self {
            Self::ReadCode { instructions, .. } | Self::ReadDir { instructions, .. } => {
                instructions.as_deref()
            }
            _ => None,
        }
    }

    pub fn owned_instructions(&self) -> Option<Vec<InstructionBlock>> {
        self.instructions()
            .filter(|b| !b.is_empty())
            .map(|b| b.to_vec())
    }

    pub fn is_markdown(&self) -> bool {
        matches!(self, Self::Markdown(_))
    }

    pub fn structured_display_text(&self) -> Option<String> {
        match self {
            Self::Diff { .. }
            | Self::ReadCode { .. }
            | Self::ReadDir { .. }
            | Self::WriteCode { .. }
            | Self::GrepResult { .. }
            | Self::TodoList(_)
            | Self::Findings(_)
            | Self::ReviewResult { .. } => Some(self.as_display_text()),
            _ => None,
        }
    }

    /// Compressed text for LLM consumption. Detects content type and applies
    /// compression when the output exceeds trivial size.
    pub fn as_text_for_llm(&self, config: &CompressionConfig) -> String {
        let raw = self.as_text();
        if !config.enabled || raw.len() < 200 || self.skip_compress() {
            return raw;
        }
        let ct = compression::detect_content_type(&raw);
        compression::compress(&raw, ct, &compression::CompressionConfig::from(config))
    }

    pub(crate) fn skip_compress(&self) -> bool {
        match self {
            Self::ReadCode { no_compress, .. } | Self::Batch { no_compress, .. } => *no_compress,
            _ => false,
        }
    }

    pub fn is_empty_result(&self) -> bool {
        match self {
            Self::GrepResult { entries } => entries.is_empty(),
            Self::ReadDir { text, .. } => text.is_empty(),
            Self::Plain(text) | Self::Markdown(text) => text.is_empty(),
            Self::Findings(f) | Self::ReviewResult { findings: f, .. } => f.is_empty(),

            _ => false,
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Self::Diff { summary, .. } => summary.clone(),
            Self::TodoList(_) => "ok".into(),
            Self::Findings(findings) => findings_text(findings),
            Self::ReviewResult { findings, verdict } => {
                let mut out = findings_text(findings);
                if !verdict.is_empty() {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str("## Verdict\n\n");
                    out.push_str(verdict);
                }
                out
            }
            Self::ReadCode { instructions, .. } | Self::ReadDir { instructions, .. } => {
                let mut out = self.as_display_text();
                if let Some(blocks) = instructions {
                    append_instructions(&mut out, blocks);
                }
                out
            }
            _ => self.as_display_text(),
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            Self::Plain(s) | Self::Markdown(s) => s.clone(),

            Self::ReadDir { text, .. } => text.clone(),
            Self::ReadCode {
                start_line,
                lines,
                total_lines,
                ..
            } => {
                let mut out: String = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {line}", start_line + i))
                    .collect::<Vec<_>>()
                    .join("\n");
                let remaining = lines_remaining_after(*total_lines, *start_line, lines.len());
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n\n...\n\nTruncated lines: {}-{}. Use offset={} to read further.",
                        start_line + lines.len(),
                        total_lines,
                        start_line + lines.len(),
                    ));
                }
                out
            }
            Self::Diff {
                path,
                before,
                after,
                summary,
            } => crate::diff::unified_text(
                before,
                after,
                summary,
                &crate::tools::relative_path(path),
            ),
            Self::TodoList(items) => {
                if items.is_empty() {
                    return "No tasks.".into();
                }
                let mut out = String::new();
                for (depth, node) in flatten_task_tree(items) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    let indent = "  ".repeat(depth);
                    let id = if node.id.is_empty() {
                        String::new()
                    } else {
                        format!("{} ", node.id)
                    };
                    let owner = node
                        .owner
                        .as_deref()
                        .map(|o| format!(" (@{o})"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "{indent}{id}{} {}{owner}",
                        node.status.marker(),
                        node.content
                    ));
                }
                out
            }
            Self::WriteCode {
                path, byte_count, ..
            } => {
                let display = crate::tools::relative_path(path);
                format!("wrote {byte_count} bytes to {display}")
            }
            Self::GrepResult { entries } => {
                let mut out = String::new();
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&entry.path);
                    out.push(':');
                    let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);
                    for (gi, group) in entry.groups.iter().enumerate() {
                        if gi > 0 && has_context {
                            out.push_str("\n  --");
                        }
                        for line in &group.lines {
                            let sep = if line.is_match { ":" } else { " " };
                            let _ = write!(out, "\n  {}{sep} {}", line.line_nr, line.text);
                        }
                    }
                }
                out
            }
            Self::Batch { text, .. } => text.clone(),
            Self::Instructions { blocks } => {
                let mut out = String::new();
                append_instructions(&mut out, blocks);
                out
            }
            Self::Findings(findings) => findings_display(findings),
            Self::ReviewResult { findings, verdict } => {
                let mut out = findings_display(findings);
                if !verdict.is_empty() {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(verdict);
                }
                out
            }
        }
    }
}

fn findings_text(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let mut out = format!("## Review Findings ({} issue{})\n\n", findings.len(), if findings.len() == 1 { "" } else { "s" });
    for f in findings {
        let confidence_pct = (f.confidence.clamp(0.0, 1.0) * 100.0) as u8;
        let _ = writeln!(out, "[{}] {}", f.priority, f.title);
        let _ = writeln!(out, "  Location: {}:{}-{} | Confidence: {}%", f.file_path, f.line_start, f.line_end, confidence_pct);
        out.push_str(f.body.trim());
        out.push('\n');
        if !f.rule_ids.is_empty() {
            let _ = writeln!(out, "  Rules: {}", f.rule_ids.join(", "));
        }
        if let Some(ref fix) = f.suggestion {
            out.push_str("  Fix: ");
            out.push_str(fix.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn findings_display(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let mut out = format!("Findings ({} issue{})\n", findings.len(), if findings.len() == 1 { "" } else { "s" });
    for f in findings {
        let confidence_pct = (f.confidence.clamp(0.0, 1.0) * 100.0) as u8;
        let _ = writeln!(out, "[{}] {}", f.priority, f.title);
        let _ = writeln!(out, "  {}:{}-{} | {}%", f.file_path, f.line_start, f.line_end, confidence_pct);
        out.push_str(f.body.trim());
        out.push('\n');
        if let Some(ref fix) = f.suggestion {
            out.push_str("  Fix: ");
            out.push_str(fix.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub summary: String,
    pub render_header: Option<BufferSnapshot>,
    pub annotation: Option<String>,
    pub input: Option<ToolInput>,
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<ToolOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDoneEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub output: ToolOutput,
    pub is_error: bool,
}

const UNKNOWN_TOOL: &str = "unknown";

impl ToolDoneEvent {
    pub fn error(id: String, message: impl Into<String>) -> Self {
        Self {
            id,
            tool: Arc::from(UNKNOWN_TOOL),
            output: ToolOutput::Plain(message.into()),
            is_error: true,
        }
    }

    pub fn written_path(&self) -> Option<&str> {
        if self.is_error {
            return None;
        }
        self.output.written_path()
    }

    pub fn wrote_to(&self, plan_path: &Path) -> bool {
        self.written_path()
            .is_some_and(|wp| Path::new(wp) == plan_path)
    }
}

pub fn tool_results(results: Vec<ToolDoneEvent>, config: &CompressionConfig) -> Message {
    Message {
        role: Role::User,
        content: results
            .into_iter()
            .map(|r| ContentBlock::ToolResult {
                tool_use_id: r.id,
                content: r.output.as_text_for_llm(config),
                is_error: r.is_error,
            })
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionContext {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolPending {
        id: String,
        name: String,
    },
    ToolStart(Box<ToolStartEvent>),
    /// `content` is the full accumulated output so far, not a delta.
    /// Producers must accumulate into a growing buffer and send the whole thing each flush.
    ToolOutput {
        id: String,
        content: String,
    },
    ToolDone(Box<ToolDoneEvent>),
    BatchProgress(Box<BatchProgressEvent>),
    TurnComplete(Box<TurnCompleteEvent>),
    ToolResultsSubmitted {
        message: Box<Message>,
    },
    QueueItemConsumed {
        text: String,
        image_count: usize,
    },
    Done {
        usage: TokenUsage,
        num_turns: u32,
        stop_reason: Option<StopReason>,
    },
    AutoCompacting,
    Info {
        message: String,
    },
    ModelEscalation {
        from: String,
        to: String,
    },
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    Error {
        message: String,
    },
    PermissionRequest {
        id: String,
        tool: String,
        scopes: Vec<String>,
        #[serde(default)]
        context: PermissionContext,
    },
    AuthRequired,
    SubagentHistory {
        tool_use_id: String,
        messages: Vec<Message>,
    },
    ToolSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    ToolHeaderSnapshot {
        id: String,
        snapshot: BufferSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_gen: Option<u64>,
    },
    LiveToolBuf {
        id: String,
        body: Arc<SharedBuf>,
    },
    #[cfg(feature = "onnx")]
    StagnationDetected {
        similarity: f32,
    },
}

/// Append-only buffer for streaming tool output to the UI. Writers append
/// under a Mutex, readers get a cheap Arc clone via `read_if_dirty()`.
pub struct SharedBuf {
    committed: Mutex<Arc<Vec<SnapshotLine>>>,
    dirty: AtomicBool,
}

impl SharedBuf {
    pub fn new() -> Self {
        Self {
            committed: Mutex::new(Arc::new(Vec::new())),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn append(&self, line: SnapshotLine) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut guard).push(line);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn set_lines(&self, lines: Vec<SnapshotLine>) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(lines);
        drop(guard);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read_if_dirty(&self) -> Option<Arc<Vec<SnapshotLine>>> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(&guard))
    }

    pub fn take(&self) -> BufferSnapshot {
        self.dirty.store(false, Ordering::Release);
        let guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        BufferSnapshot::from_arc(Arc::clone(&guard))
    }
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SharedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBuf").finish_non_exhaustive()
    }
}

impl Serialize for SharedBuf {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_unit()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BufferSnapshot {
    pub lines: Arc<Vec<SnapshotLine>>,
}

impl BufferSnapshot {
    pub fn from_arc(lines: Arc<Vec<SnapshotLine>>) -> Self {
        Self { lines }
    }

    pub fn first_line_text(&self) -> String {
        self.lines
            .first()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotLine {
    pub spans: Vec<SnapshotSpan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpan {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SpanStyle {
    #[default]
    Default,
    Named(String),
    Inline(InlineStyle),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineStyle {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

#[derive(Debug, Serialize)]
pub struct TurnCompleteEvent {
    pub message: Message,
    pub usage: TokenUsage,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BatchProgressEvent {
    pub batch_id: String,
    pub index: usize,
    pub tool: String,
    pub status: BatchToolStatus,
    pub output: Option<ToolOutput>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentInfo {
    pub parent_tool_use_id: String,
    #[serde(rename = "parent_name")]
    pub name: String,
    #[serde(rename = "parent_prompt", skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "parent_model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip)]
    pub answer_tx: Option<flume::Sender<String>>,
}

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: Sender<Envelope>,
    run_id: u64,
}

impl EventSender {
    pub fn new(tx: Sender<Envelope>, run_id: u64) -> Self {
        Self { tx, run_id }
    }

    pub fn send(&self, event: impl Into<AgentEvent>) -> Result<(), AgentError> {
        self.tx
            .try_send(Envelope {
                event: event.into(),
                subagent: None,
                run_id: self.run_id,
            })
            .map_err(|_| AgentError::Channel)
    }

    pub fn send_envelope(&self, envelope: Envelope) -> Result<(), AgentError> {
        self.tx.try_send(envelope).map_err(|_| AgentError::Channel)
    }

    pub fn try_send(&self, event: impl Into<AgentEvent>) {
        let _ = self.tx.try_send(Envelope {
            event: event.into(),
            subagent: None,
            run_id: self.run_id,
        });
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    pub fn raw_tx(&self) -> &Sender<Envelope> {
        &self.tx
    }
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub event: AgentEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    pub run_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn as_display_text_diff_renders_unified_text() {
        let output = ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "keep\nold\n".into(),
            after: "keep\nnew\n".into(),
            summary: "Updated value".into(),
        };
        let display = output.as_display_text();
        assert!(display.starts_with("Updated value"));
        assert!(display.contains("--- src/main.rs"));
        assert!(display.contains("+++ src/main.rs"));
        assert!(display.contains("  keep"));
        assert!(display.contains("- old"));
        assert!(display.contains("+ new"));
        assert_eq!(output.as_text(), "Updated value");
    }

    #[test]
    fn as_display_text_todolist_formats_hierarchy() {
        let output = ToolOutput::TodoList(vec![
            TaskNode {
                id: "T1".into(),
                parent: None,
                content: "done".into(),
                status: TodoStatus::Completed,
                owner: None,
            },
            TaskNode {
                id: "T1.1".into(),
                parent: Some("T1".into()),
                content: "wip".into(),
                status: TodoStatus::InProgress,
                owner: Some("research".into()),
            },
            TaskNode {
                id: "T2".into(),
                parent: None,
                content: "todo".into(),
                status: TodoStatus::Pending,
                owner: None,
            },
        ]);
        let display = output.as_display_text();
        assert!(display.contains("T1 [✓] done"));
        assert!(display.contains("  T1.1 [•] wip (@research)"));
        assert!(display.contains("T2 [ ] todo"));
        assert_eq!(output.as_text(), "ok");
    }

    #[test_case("T1"; "single")]
    #[test_case("T1.1"; "two levels")]
    #[test_case("T1.1.2"; "three levels")]
    #[test_case("T10"; "two digit")]
    fn task_id_valid(id: &str) {
        assert!(TaskNode::is_valid_id(id));
    }

    #[test_case("T0"; "zero root")]
    #[test_case("T1.0"; "zero child")]
    #[test_case("T1..1"; "empty component")]
    #[test_case("T01"; "leading zero")]
    #[test_case("T"; "no digits")]
    #[test_case("" ; "empty")]
    #[test_case("1"; "missing t prefix")]
    fn task_id_invalid(id: &str) {
        assert!(!TaskNode::is_valid_id(id));
    }

    #[test]
    fn flatten_task_tree_orders_by_depth() {
        let items = vec![
            TaskNode {
                id: "T2".into(),
                parent: None,
                content: "two".into(),
                status: TodoStatus::Pending,
                owner: None,
            },
            TaskNode {
                id: "T1".into(),
                parent: None,
                content: "one".into(),
                status: TodoStatus::Pending,
                owner: None,
            },
            TaskNode {
                id: "T1.1".into(),
                parent: Some("T1".into()),
                content: "one-one".into(),
                status: TodoStatus::Pending,
                owner: None,
            },
        ];
        let flat: Vec<(usize, String)> = flatten_task_tree(&items)
            .into_iter()
            .map(|(d, n)| (d, n.content.clone()))
            .collect();
        assert_eq!(
            flat,
            vec![
                (0, "two".into()),
                (0, "one".into()),
                (1, "one-one".into())
            ]
        );
    }

    #[test]
    fn task_list_round_trips_through_serde() {
        let output = ToolOutput::TodoList(vec![
            TaskNode {
                id: "T1".into(),
                parent: None,
                content: "root".into(),
                status: TodoStatus::Completed,
                owner: None,
            },
            TaskNode {
                id: "T1.1".into(),
                parent: Some("T1".into()),
                content: "child".into(),
                status: TodoStatus::InProgress,
                owner: Some("general".into()),
            },
        ]);
        let json = serde_json::to_string(&output).unwrap();
        let back: ToolOutput = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn as_text_grep_result_multi_file() {
        let output = ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/a.rs".into(),
                    groups: vec![
                        GrepMatchGroup::single(3, "fn foo()"),
                        GrepMatchGroup::single(10, "fn bar()"),
                    ],
                },
                GrepFileEntry {
                    path: "src/b.rs".into(),
                    groups: vec![GrepMatchGroup::single(1, "use crate")],
                },
            ],
        };
        let text = output.as_text();
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("3: fn foo()"));
        assert!(text.contains("10: fn bar()"));
        assert!(text.contains("src/b.rs"));
        assert!(text.contains("1: use crate"));
    }

    #[test]
    fn as_text_grep_result_with_context() {
        let output = ToolOutput::GrepResult {
            entries: vec![GrepFileEntry {
                path: "src/a.rs".into(),
                groups: vec![
                    GrepMatchGroup {
                        lines: vec![
                            GrepLine::context(2, "let x = 1;"),
                            GrepLine::matched(3, "fn foo()"),
                            GrepLine::context(4, "let y = 2;"),
                        ],
                    },
                    GrepMatchGroup::single(20, "fn bar()"),
                ],
            }],
        };
        let text = output.as_text();
        assert!(text.contains("2  let x = 1;"), "context before: {text}");
        assert!(text.contains("3: fn foo()"), "match line: {text}");
        assert!(text.contains("4  let y = 2;"), "context after: {text}");
        assert!(text.contains("--"), "group separator: {text}");
        assert!(text.contains("20: fn bar()"), "second group: {text}");
    }

    #[test_case(ToolOutput::WriteCode { path: "src/lib.rs".into(), byte_count: 10, lines: vec![] }, Some("src/lib.rs") ; "write_code")]
    #[test_case(ToolOutput::Diff { path: "src/lib.rs".into(), before: String::new(), after: String::new(), summary: String::new() }, Some("src/lib.rs") ; "diff")]
    #[test_case(ToolOutput::Plain("ok".into()), None ; "non_write_variant")]
    fn output_written_path(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.written_path(), expected);
    }

    #[test]
    fn tool_results_builds_message_with_tool_result_blocks() {
        let msg = tool_results(vec![
            ToolDoneEvent {
                id: "t1".into(),
                tool: Arc::from("bash"),
                output: ToolOutput::Plain("ok".into()),
                is_error: false,
            },
            ToolDoneEvent {
                id: "t2".into(),
                tool: Arc::from("read"),
                output: ToolOutput::Plain("fail".into()),
                is_error: true,
            },
        ], &CompressionConfig::default());
        assert!(matches!(msg.role, Role::User));
        assert_eq!(msg.content.len(), 2);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && !is_error)
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t2" && *is_error)
        );
    }

    #[test_case(
        10,
        vec!["fn foo()".into(), "fn bar()".into()],
        Some(vec![InstructionBlock { path: "AGENTS.md".into(), content: "do stuff".into() }]),
        "10: fn foo()\n11: fn bar()\n\n...\n\nTruncated lines: 12-100. Use offset=12 to read further."
        ; "with_instructions"
    )]
    #[test_case(
        1,
        vec!["line1".into()],
        None,
        "1: line1\n\n...\n\nTruncated lines: 2-100. Use offset=2 to read further."
        ; "without_instructions"
    )]
    fn read_code_display_text(
        start_line: usize,
        lines: Vec<String>,
        instructions: Option<Vec<InstructionBlock>>,
        expected: &str,
    ) {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line,
            lines,
            total_lines: 100,
            instructions,
            no_compress: false,
        };
        assert_eq!(output.as_display_text(), expected);
    }

    #[test]
    fn read_code_as_text_includes_instructions() {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line: 1,
            lines: vec!["fn main()".into()],
            total_lines: 1,
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
            no_compress: false,
        };
        let text = output.as_text();
        assert!(text.contains("1: fn main()"));
        assert!(text.contains("Instructions from: AGENTS.md"));
        assert!(text.contains("do stuff"));
    }

    #[test]
    fn wrote_to_checks_path_and_error_flag() {
        let ok_event = ToolDoneEvent {
            id: "id".into(),
            tool: Arc::from("write"),
            output: ToolOutput::WriteCode {
                path: "/plans/slug.md".into(),
                byte_count: 10,
                lines: vec![],
            },
            is_error: false,
        };
        assert!(ok_event.wrote_to(Path::new("/plans/slug.md")));
        assert!(!ok_event.wrote_to(Path::new("/plans/other.md")));

        let err_event = ToolDoneEvent {
            is_error: true,
            ..ok_event
        };
        assert!(!err_event.wrote_to(Path::new("/plans/slug.md")));
    }

    #[test]
    fn read_code_backward_compat_deserialization() {
        let json = r#"{"ReadCode":{"path":"a.rs","start_line":1,"lines":["x"]}}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match output {
            ToolOutput::ReadCode {
                total_lines,
                instructions,
                ..
            } => {
                assert_eq!(total_lines, 0);
                assert!(instructions.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test_case(100, 10, 2, 89 ; "middle_of_file")]
    #[test_case(100, 1, 1, 99  ; "first_line_only")]
    #[test_case(5, 1, 5, 0     ; "all_lines_shown")]
    #[test_case(5, 1, 2, 3     ; "partial_from_start")]
    #[test_case(5, 3, 3, 0     ; "partial_to_end")]
    #[test_case(0, 1, 1, 0     ; "backward_compat_total_zero")]
    #[test_case(0, 1, 0, 0     ; "empty_lines_total_zero")]
    #[test_case(10, 10, 1, 0   ; "last_line")]
    fn lines_remaining(total: usize, start: usize, shown: usize, expected: usize) {
        assert_eq!(lines_remaining_after(total, start, shown), expected);
    }

    fn line(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        }
    }

    #[test]
    fn shared_buf_lifecycle() {
        let buf = SharedBuf::new();

        assert!(buf.is_empty());
        assert!(buf.read_if_dirty().is_none());

        for i in 0..3 {
            buf.append(line(&format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);

        let snap = buf.read_if_dirty().expect("dirty after appends");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].spans[0].text, "l0");
        assert!(buf.read_if_dirty().is_none(), "clean after read");

        buf.append(line("l3"));
        let _ = buf.take();
        assert!(buf.read_if_dirty().is_none(), "take clears dirty");
    }

    #[test]
    fn shared_buf_arc_snapshot_isolation() {
        let buf = SharedBuf::new();
        buf.append(line("a"));
        buf.append(line("b"));
        let snap = buf.read_if_dirty().unwrap();
        buf.append(line("c"));
        assert_eq!(snap.len(), 2, "held Arc must not see new appends");
        let snap2 = buf.read_if_dirty().unwrap();
        assert_eq!(snap2.len(), 3);
    }

    #[test]
    fn shared_buf_poisoned_mutex_recovery() {
        let buf = Arc::new(SharedBuf::new());
        let buf2 = Arc::clone(&buf);
        let h = std::thread::spawn(move || {
            let _guard = buf2.committed.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        buf.append(SnapshotLine { spans: vec![] });
    }

    #[test]
    fn buffer_snapshot_first_line_text() {
        let empty = BufferSnapshot {
            lines: Arc::new(vec![]),
        };
        assert_eq!(empty.first_line_text(), "");

        let multi = BufferSnapshot {
            lines: Arc::new(vec![SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "hello ".into(),
                        style: SpanStyle::Default,
                    },
                    SnapshotSpan {
                        text: "world".into(),
                        style: SpanStyle::Named("bold".into()),
                    },
                ],
            }]),
        };
        assert_eq!(multi.first_line_text(), "hello world");
    }

    #[test_case(SpanStyle::Default ; "default")]
    #[test_case(SpanStyle::Named("comment".into()) ; "named")]
    #[test_case(SpanStyle::Inline(InlineStyle {
        fg: Some((255, 0, 0)),
        bg: None,
        bold: true,
        italic: false,
        underline: true,
        dim: false,
        strikethrough: false,
        reversed: true,
    }) ; "inline")]
    fn snapshot_span_serde_roundtrip(style: SpanStyle) {
        let span = SnapshotSpan {
            text: "test".into(),
            style,
        };
        let json = serde_json::to_string(&span).unwrap();
        let parsed: SnapshotSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, span);
    }

    #[test_case("", true  ; "plain_output_is_empty_for_empty_string")]
    #[test_case("a.rs\nb.rs", false ; "plain_output_not_empty_for_content")]
    fn plain_output_is_empty(text: &str, expected: bool) {
        assert_eq!(ToolOutput::Plain(text.into()).is_empty_result(), expected);
    }

    #[test]
    fn as_text_for_llm_short_output_not_compressed() {
        let output = ToolOutput::Plain("short content".into());
        let config = CompressionConfig { enabled: true, ..CompressionConfig::default() };
        assert_eq!(output.as_text_for_llm(&config), "short content");
    }

    #[test]
    fn as_text_for_llm_disabled_returns_raw() {
        let long = "fn foo()\n".repeat(50);
        let output = ToolOutput::Plain(long.clone());
        let config = CompressionConfig { enabled: false, ..CompressionConfig::default() };
        assert_eq!(output.as_text_for_llm(&config), long);
    }
}
