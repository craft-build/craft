use std::collections::HashMap;
use std::sync::Arc;

use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

pub trait ValidNames: IntoEnumIterator + std::fmt::Display {
    fn valid_names() -> String {
        Self::iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
pub const SYSTEM_SMALL_PROMPT: &str = include_str!("prompts/system_small.md");
pub const PLAN_PROMPT: &str = include_str!("prompts/plan.md");
pub const FLOW_PROMPT: &str = include_str!("prompts/flow.md");

/// Substitute `{workstream_id}` into [`FLOW_PROMPT`] for the given mode. Returns
/// `None` outside Flow mode. Shared by `build_system_prompt` (the root agent)
/// and `run_subagent` (stage subagents) so both receive identical Flow context,
/// which keeps the section in one place and the workstream id substitution
/// consistent.
pub fn flow_section(mode: &crate::AgentMode) -> Option<String> {
    let workstream = mode.flow_workstream()?;
    let vars = crate::template::Vars::new().set("{workstream_id}", workstream.to_string());
    Some(vars.apply(FLOW_PROMPT).into_owned())
}

pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");
pub const COMPACTION_TARGETED_USER: &str = include_str!("prompts/compaction_targeted_user.md");
pub const DREAM_PROMPT: &str = include_str!("prompts/dream.md");
pub const DISTILL_PROMPT: &str = include_str!("prompts/distill.md");
pub const CHECKPOINT_PROMPT: &str = include_str!("prompts/checkpoint.md");
pub const WIKI_INIT_PROMPT: &str = include_str!("prompts/wiki_init.md");

pub const DEFAULT_IDENTITY: &str = r#"You are Craft, an interactive CLI coding agent. Use the tools available to assist the user with software engineering tasks. Complete tasks successfully while minimizing token usage and tool calls to avoid context bloat.

You must NEVER generate or guess URLs unless they are for helping the user with programming."#;

pub const DEFAULT_TONE: &str = r#"- Be concise. Your output is displayed on a CLI rendered in monospace. Use GitHub-flavored markdown.
- Only use emojis if explicitly requested.
- Add only succinct, genuinely helpful comments. A brief doc comment on a public item, or one line marking a non-obvious decision, is welcome. Do NOT restate what the code already says, narrate changes, or add section banners and per-block explanations. If a name or type already conveys the meaning, no comment is needed.
- Output text to communicate with the user; all text you output outside of tool use is displayed to the user. Only use tools to complete tasks. NEVER use bash echo or other command-line tools to communicate thoughts, explanations, diagrams, or instructions to the user. Output all communication directly in your response text instead.
- NEVER create files unless absolutely necessary. ALWAYS prefer editing existing files."#;

const NATIVE_EFFICIENT_TOOLS: &[&str] = &["batch", "code_execution", "task"];
const INSTRUCTIONS_MARKER: &str = "{{instructions}}";

pub const DEFAULT_SUBAGENT_BRIEFING: &str = r#"# Delegating to subagents
A subagent starts with none of this conversation's context. Write its prompt like a briefing for a smart colleague who just walked into the room:
- State what to accomplish and why. Describe what is already ruled out so it does not redo dead-end work.
- Give enough surrounding context for judgment calls, not just a narrow instruction.
- Lookups vs investigations: for a lookup, hand over the exact command or query. For an investigation, hand over the question. Prescribed steps become dead weight when the premise is wrong.
- Never delegate understanding. Include file paths, line numbers, and what specifically to change. Do not write "based on your findings, fix the bug."
- Give a response-length hint (e.g. "report in under 200 words") to control the return payload."#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display)]
#[strum(serialize_all = "snake_case")]
pub enum SlotKind {
    Singleton,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum Slot {
    Identity,
    Tone,
    ToolUsage,
    EfficientTools,
    SubagentBriefing,
    Conventions,
    AfterInstructions,
}

impl Slot {
    fn marker(self) -> &'static str {
        match self {
            Slot::Identity => "{{identity}}",
            Slot::Tone => "{{tone}}",
            Slot::ToolUsage => "{{tool_usage}}",
            Slot::EfficientTools => "{{efficient_tools}}",
            Slot::SubagentBriefing => "{{subagent_briefing}}",
            Slot::Conventions => "{{conventions}}",
            Slot::AfterInstructions => "{{after_instructions}}",
        }
    }

    pub fn kind(self) -> SlotKind {
        match self {
            Slot::Identity | Slot::Tone | Slot::SubagentBriefing => SlotKind::Singleton,
            Slot::ToolUsage
            | Slot::EfficientTools
            | Slot::Conventions
            | Slot::AfterInstructions => SlotKind::Aggregate,
        }
    }

    pub fn default_content(self) -> Option<&'static str> {
        match self {
            Slot::Identity => Some(DEFAULT_IDENTITY),
            Slot::Tone => Some(DEFAULT_TONE),
            Slot::SubagentBriefing => Some(DEFAULT_SUBAGENT_BRIEFING),
            _ => None,
        }
    }

    pub fn names_for_kind(kind: SlotKind) -> String {
        Self::iter()
            .filter(|s| s.kind() == kind)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum PromptId {
    System,
    Research,
    General,
}

impl PromptId {
    pub const ALL: &[PromptId] = &[PromptId::System, PromptId::Research, PromptId::General];
}

impl ValidNames for Slot {}
impl ValidNames for PromptId {}

pub struct SlotEntry {
    pub plugin: Arc<str>,
    pub content: String,
}

#[derive(Default)]
pub struct ResolvedSlots {
    entries: HashMap<(PromptId, Slot), Vec<SlotEntry>>,
}

impl ResolvedSlots {
    pub fn get(&self, prompt: PromptId, slot: Slot) -> &[SlotEntry] {
        self.entries
            .get(&(prompt, slot))
            .map(|v| v.as_slice())
            .unwrap_or_default()
    }

    pub fn insert(&mut self, prompt: PromptId, slot: Slot, entry: SlotEntry) {
        self.entries.entry((prompt, slot)).or_default().push(entry);
    }
}

impl PromptId {
    fn template(self) -> &'static str {
        match self {
            PromptId::System => SYSTEM_PROMPT,
            PromptId::Research => RESEARCH_PROMPT,
            PromptId::General => GENERAL_PROMPT,
        }
    }

    pub fn has_slot(self, slot: Slot) -> bool {
        self.template().contains(slot.marker())
    }
}

fn render_slot(slots: &ResolvedSlots, prompt: PromptId, slot: Slot) -> String {
    if slot == Slot::EfficientTools {
        return render_efficient_tools(slots, prompt);
    }
    let entries = slots.get(prompt, slot);
    match slot.kind() {
        SlotKind::Singleton => {
            if let Some(last) = entries.last() {
                last.content.clone()
            } else if let Some(default) = slot.default_content() {
                default.to_string()
            } else {
                String::new()
            }
        }
        SlotKind::Aggregate => entries
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_efficient_tools(slots: &ResolvedSlots, prompt: PromptId) -> String {
    let extras = slots.get(prompt, Slot::EfficientTools);
    let names = NATIVE_EFFICIENT_TOOLS
        .iter()
        .copied()
        .chain(extras.iter().map(|e| e.content.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Most efficient tools: {names}.")
}

pub fn assemble(id: PromptId, slots: &ResolvedSlots, instructions: &str) -> String {
    let mut out = id.template().to_string();
    for slot in Slot::iter() {
        out = fill_marker(&out, slot.marker(), &render_slot(slots, id, slot));
    }
    out.replace(INSTRUCTIONS_MARKER, instructions)
}

pub fn assemble_raw(template: &str, slots: &ResolvedSlots, instructions: &str) -> String {
    let mut out = template.to_string();
    for slot in Slot::iter() {
        out = fill_marker(
            &out,
            slot.marker(),
            &render_slot(slots, PromptId::System, slot),
        );
    }
    out.replace(INSTRUCTIONS_MARKER, instructions)
}

const RECENCY_HEADER: &str = "<turn-context>";

/// A small, per-turn bundle of volatile facts rendered onto the latest user
/// message at request-build time. Unlike [`Slot`] content it never enters the
/// system prompt (which would break prompt-cache stability) and is never
/// persisted to [`History`](crate::agent::history::History); it is rebuilt from
/// scratch every turn and discarded after the request. This is the recency
/// channel described in `docs/feature-primacy-recency-slotting.md`.
#[derive(Debug, Clone, Default)]
pub struct RecencyFacts {
    blocks: Vec<String>,
}

impl RecencyFacts {
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    pub fn push(&mut self, block: String) {
        if !block.is_empty() {
            self.blocks.push(block);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Render the blocks under a single header. Returns an empty string when
    /// there is nothing to inject, so an empty [`RecencyFacts`] is a no-op for
    /// the request builder.
    pub fn render(&self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(64);
        out.push_str(RECENCY_HEADER);
        out.push_str("\n\n");
        out.push_str(&self.blocks.join("\n\n"));
        out
    }
}

/// Handed to a [`RecencySource`] each turn. Carries the minimal signal a
/// volatile source needs today; extend it only when a real source requires
/// more, keeping the per-turn surface small.
#[derive(Debug, Clone, Copy)]
pub struct RecencyCtx {
    pub turn: u32,
}

/// Extension point for per-turn volatile facts. The host (e.g. the Lua plugin
/// runtime) supplies one concrete implementation; the agent holds it as
/// `Option<Arc<dyn RecencySource>>` and consults it once per turn, mirroring
/// how [`InterruptSource`](crate::InterruptSource) and other host-side
/// concerns are injected. Kept in `craft-agent` (not `craft-lua`) so the
/// agent never depends on the plugin layer.
pub trait RecencySource: Send + Sync {
    fn collect(&self, ctx: &RecencyCtx) -> RecencyFacts;
}

fn fill_marker(template: &str, marker: &str, content: &str) -> String {
    if content.is_empty() {
        return template
            .replace(&format!("{marker}\n"), "")
            .replace(marker, "");
    }
    template.replace(marker, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const NATIVE_EFFICIENT_LINE: &str = "Most efficient tools: batch, code_execution, task";

    fn slots(prompt: PromptId, entries: &[(Slot, &str)]) -> ResolvedSlots {
        let mut slots = ResolvedSlots::default();
        for &(slot, content) in entries {
            slots.insert(
                prompt,
                slot,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: content.into(),
                },
            );
        }
        slots
    }

    fn at(out: &str, needle: &str) -> usize {
        out.find(needle)
            .unwrap_or_else(|| panic!("missing: {needle}"))
    }

    #[test]
    fn empty_slots_emit_template_and_native_efficient_line() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Craft"));
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}.")));
    }

    #[test]
    fn system_sections_land_in_layout_order() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::ToolUsage, "TOOL_USAGE"),
                (Slot::EfficientTools, "EXTRA_TOOL"),
                (Slot::Conventions, "CONVENTIONS"),
                (Slot::AfterInstructions, "AFTER"),
            ],
        );
        let out = assemble(PromptId::System, &s, "INSTR");
        let positions = ["TOOL_USAGE", "EXTRA_TOOL", "CONVENTIONS", "INSTR", "AFTER"]
            .map(|needle| at(&out, needle));
        assert!(
            positions.is_sorted(),
            "sections out of layout order ({positions:?}):\n{out}"
        );
    }

    #[test]
    fn tool_usage_hint_lands_inside_tool_usage_section() {
        const HINT: &str = "- HINT_LINE";
        let s = slots(PromptId::System, &[(Slot::ToolUsage, HINT)]);
        let out = assemble(PromptId::System, &s, "");
        let hint = at(&out, HINT);
        assert!(
            at(&out, "# Tool usage") < hint,
            "hint before its section:\n{out}"
        );
        assert!(
            hint < at(&out, "# Conventions"),
            "hint leaked past section:\n{out}"
        );
    }

    #[test]
    fn efficient_tools_extras_join_native_list() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::EfficientTools, "outline"),
                (Slot::EfficientTools, "foo"),
            ],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, outline, foo.")));
    }

    #[test]
    fn same_slot_preserves_insertion_order() {
        let s = slots(
            PromptId::System,
            &[(Slot::ToolUsage, "FIRST"), (Slot::ToolUsage, "SECOND")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(at(&out, "FIRST") < at(&out, "SECOND"));
    }

    #[test]
    fn after_instructions_only_reaches_system() {
        let mut s = ResolvedSlots::default();
        for &pid in PromptId::ALL {
            s.insert(
                pid,
                Slot::AfterInstructions,
                SlotEntry {
                    plugin: Arc::from("p"),
                    content: "AFTER".into(),
                },
            );
        }
        assert!(assemble(PromptId::System, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::Research, &s, "").contains("AFTER"));
        assert!(!assemble(PromptId::General, &s, "").contains("AFTER"));
    }

    #[test]
    fn research_drops_conventions_but_keeps_efficient_extras() {
        let s = slots(
            PromptId::Research,
            &[
                (Slot::Conventions, "DROPPED"),
                (Slot::EfficientTools, "EXTRA"),
            ],
        );
        let out = assemble(PromptId::Research, &s, "");
        assert!(!out.contains("DROPPED"));
        assert!(out.contains(&format!("{NATIVE_EFFICIENT_LINE}, EXTRA.")));
    }

    #[test_case(PromptId::System, Slot::ToolUsage, true ; "system_tool_usage")]
    #[test_case(PromptId::System, Slot::EfficientTools, true ; "system_efficient")]
    #[test_case(PromptId::System, Slot::Conventions, true ; "system_conventions")]
    #[test_case(PromptId::System, Slot::AfterInstructions, true ; "system_after")]
    #[test_case(PromptId::System, Slot::SubagentBriefing, true ; "system_subagent_briefing")]
    #[test_case(PromptId::System, Slot::Identity, true ; "system_identity")]
    #[test_case(PromptId::System, Slot::Tone, true ; "system_tone")]
    #[test_case(PromptId::Research, Slot::Conventions, false ; "research_no_conventions")]
    #[test_case(PromptId::Research, Slot::AfterInstructions, false ; "research_no_after")]
    #[test_case(PromptId::Research, Slot::Identity, false ; "research_no_identity")]
    #[test_case(PromptId::Research, Slot::Tone, false ; "research_no_tone")]
    #[test_case(PromptId::General, Slot::AfterInstructions, false ; "general_no_after")]
    #[test_case(PromptId::General, Slot::Identity, false ; "general_no_identity")]
    #[test_case(PromptId::General, Slot::Tone, false ; "general_no_tone")]
    fn has_slot(prompt: PromptId, slot: Slot, expected: bool) {
        assert_eq!(prompt.has_slot(slot), expected);
    }

    #[test_case("after_instructions", Some(Slot::AfterInstructions) ; "valid_slot")]
    #[test_case("tool_usagee", None ; "typo_slot")]
    #[test_case("identity", Some(Slot::Identity) ; "identity_slot")]
    #[test_case("tone", Some(Slot::Tone) ; "tone_slot")]
    fn slot_parse_is_plugin_contract(input: &str, expected: Option<Slot>) {
        assert_eq!(input.parse::<Slot>().ok(), expected);
    }

    #[test_case("system", Some(PromptId::System) ; "valid_prompt")]
    #[test_case("systm", None ; "typo_prompt")]
    fn prompt_parse_is_plugin_contract(input: &str, expected: Option<PromptId>) {
        assert_eq!(input.parse::<PromptId>().ok(), expected);
    }

    #[test_case(Slot::Identity, SlotKind::Singleton ; "identity_singleton")]
    #[test_case(Slot::Tone, SlotKind::Singleton ; "tone_singleton")]
    #[test_case(Slot::Conventions, SlotKind::Aggregate ; "conventions_aggregate")]
    #[test_case(Slot::ToolUsage, SlotKind::Aggregate ; "tool_usage_aggregate")]
    #[test_case(Slot::EfficientTools, SlotKind::Aggregate ; "efficient_aggregate")]
    #[test_case(Slot::SubagentBriefing, SlotKind::Singleton ; "subagent_briefing_singleton")]
    #[test_case(Slot::AfterInstructions, SlotKind::Aggregate ; "after_aggregate")]
    fn slot_kind_matches_expectations(slot: Slot, expected: SlotKind) {
        assert_eq!(slot.kind(), expected);
    }

    #[test]
    fn singleton_default_used_when_empty() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Craft"));
    }

    #[test]
    fn subagent_briefing_default_renders_and_is_overridable() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(
            out.contains("# Delegating to subagents"),
            "default briefing missing from system prompt:\n{out}"
        );
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );

        let s = slots(
            PromptId::System,
            &[(Slot::SubagentBriefing, "- CUSTOM_BRIEFING")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(
            out.contains("- CUSTOM_BRIEFING"),
            "plugin briefing not applied:\n{out}"
        );
        assert!(
            !out.contains("# Delegating to subagents"),
            "default briefing should be replaced by plugin entry:\n{out}"
        );
    }

    #[test]
    fn singleton_entry_replaces_default() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("user"),
                content: "Custom identity".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Custom identity"));
        assert!(!out.contains("You are Craft"));
    }

    #[test]
    fn singleton_last_entry_wins() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("first"),
                content: "FIRST".into(),
            },
        );
        s.insert(
            PromptId::System,
            Slot::Identity,
            SlotEntry {
                plugin: Arc::from("second"),
                content: "SECOND".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("SECOND"));
        assert!(!out.contains("FIRST"));
        assert!(!out.contains("You are Craft"));
    }

    #[test]
    fn identity_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Identity));
        assert!(!PromptId::Research.has_slot(Slot::Identity));
        assert!(!PromptId::General.has_slot(Slot::Identity));
    }

    #[test]
    fn tone_only_in_system_not_subagents() {
        assert!(PromptId::System.has_slot(Slot::Tone));
        assert!(!PromptId::Research.has_slot(Slot::Tone));
        assert!(!PromptId::General.has_slot(Slot::Tone));
    }

    #[test]
    fn conventions_entry_appends_to_template_defaults() {
        let mut s = ResolvedSlots::default();
        s.insert(
            PromptId::System,
            Slot::Conventions,
            SlotEntry {
                plugin: Arc::from("plugin"),
                content: "- Extra rule".into(),
            },
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Never assume a library is available"));
        assert!(out.contains("- Extra rule"));
    }

    #[test]
    fn recency_facts_empty_renders_nothing() {
        let facts = RecencyFacts::new();
        assert!(facts.is_empty());
        assert_eq!(facts.render(), "");
    }

    #[test]
    fn recency_facts_push_empty_block_is_ignored() {
        let mut facts = RecencyFacts::new();
        facts.push(String::new());
        assert!(facts.is_empty());
    }

    #[test]
    fn recency_facts_renders_header_and_joined_blocks() {
        let mut facts = RecencyFacts::new();
        facts.push("state: clean".into());
        facts.push("todo: 3 items".into());
        let rendered = facts.render();
        assert!(rendered.starts_with("<turn-context>\n\n"));
        assert!(rendered.contains("state: clean\n\ntodo: 3 items"));
        assert!(!facts.is_empty());
    }
}
