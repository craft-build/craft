//! Prompt slot system: templated system/research/general prompts whose
//! sections are filled from named slots (ported from Craft's
//! `craft-agent/src/prompt.rs`). Slot entries are collected per prompt and
//! slot; singleton slots take the last entry (or the default content),
//! aggregate slots join every entry. The plugin runtime that overrides slots
//! (J.1) is not ported yet — today callers pass empty slots and get the
//! defaults.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
pub const RESEARCH_PROMPT: &str = include_str!("prompts/research.md");
pub const GENERAL_PROMPT: &str = include_str!("prompts/general.md");
pub const COMPACTION_SYSTEM: &str = include_str!("prompts/compaction.md");
pub const COMPACTION_USER: &str = include_str!("prompts/compaction_user.md");

pub const DEFAULT_IDENTITY: &str = r#"You are Craft, an interactive CLI coding agent. Use the tools available to assist the user with software engineering tasks. Complete tasks successfully while minimizing token usage and tool calls to avoid context bloat.

You must NEVER generate or guess URLs unless they are for helping the user with programming."#;

pub const DEFAULT_TONE: &str = r#"- Be concise. Your output is displayed on a CLI rendered in monospace. Use GitHub-flavored markdown.
- Only use emojis if explicitly requested.
- Add only succinct, genuinely helpful comments. A brief doc comment on a public item, or one line marking a non-obvious decision, is welcome. Do NOT restate what the code already says, narrate changes, or add section banners and per-block explanations. If a name or type already conveys the meaning, no comment is needed.
- Output text to communicate with the user; all text you output outside of tool use is displayed to the user. Only use tools to complete tasks. NEVER use bash echo or other command-line tools to communicate thoughts, explanations, diagrams, or instructions to the user. Output all communication directly in your response text instead.
- NEVER create files unless absolutely necessary. ALWAYS prefer editing existing files."#;

pub const DEFAULT_SUBAGENT_BRIEFING: &str = r#"# Delegating to subagents
A subagent starts with none of this conversation's context. Write its prompt like a briefing for a smart colleague who just walked into the room:
- State what to accomplish and why. Describe what is already ruled out so it does not redo dead-end work.
- Give enough surrounding context for judgment calls, not just a narrow instruction.
- Lookups vs investigations: for a lookup, hand over the exact command or query. For an investigation, hand over the question. Prescribed steps become dead weight when the premise is wrong.
- Never delegate understanding. Include file paths, line numbers, and what specifically to change. Do not write "based on your findings, fix the bug."
- Give a response-length hint (e.g. "report in under 200 words") to control the return payload."#;

/// Tools the harness dispatches natively that agents should prefer. Empty
/// until `batch`/`code_execution`/`task` are ported; grow this list as they
/// land so the assembled prompt only ever names real tools.
const NATIVE_EFFICIENT_TOOLS: &[&str] = &[];
const INSTRUCTIONS_MARKER: &str = "{{instructions}}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotKind {
    Singleton,
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    pub const ALL: &[Slot] = &[
        Slot::Identity,
        Slot::Tone,
        Slot::ToolUsage,
        Slot::EfficientTools,
        Slot::SubagentBriefing,
        Slot::Conventions,
        Slot::AfterInstructions,
    ];

    fn name(self) -> &'static str {
        match self {
            Slot::Identity => "identity",
            Slot::Tone => "tone",
            Slot::ToolUsage => "tool_usage",
            Slot::EfficientTools => "efficient_tools",
            Slot::SubagentBriefing => "subagent_briefing",
            Slot::Conventions => "conventions",
            Slot::AfterInstructions => "after_instructions",
        }
    }

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
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when a plugin (J.1, not yet ported) names an unknown slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSlotError(String);

impl fmt::Display for ParseSlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown slot {:?}, expected one of: {}",
            self.0,
            Slot::ALL
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for ParseSlotError {}

impl FromStr for Slot {
    type Err = ParseSlotError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Slot::ALL
            .iter()
            .copied()
            .find(|slot| slot.name() == s)
            .ok_or_else(|| ParseSlotError(s.into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptId {
    System,
    Research,
    General,
}

impl PromptId {
    pub const ALL: &[PromptId] = &[PromptId::System, PromptId::Research, PromptId::General];

    fn name(self) -> &'static str {
        match self {
            PromptId::System => "system",
            PromptId::Research => "research",
            PromptId::General => "general",
        }
    }

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

impl fmt::Display for PromptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePromptIdError(String);

impl fmt::Display for ParsePromptIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown prompt {:?}, expected one of: {}",
            self.0,
            PromptId::ALL
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for ParsePromptIdError {}

impl FromStr for PromptId {
    type Err = ParsePromptIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PromptId::ALL
            .iter()
            .copied()
            .find(|id| id.name() == s)
            .ok_or_else(|| ParsePromptIdError(s.into()))
    }
}

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
    let names: Vec<&str> = NATIVE_EFFICIENT_TOOLS
        .iter()
        .copied()
        .chain(extras.iter().map(|e| e.content.as_str()))
        .collect();
    if names.is_empty() {
        return String::new();
    }
    format!("Most efficient tools: {}.", names.join(", "))
}

pub fn assemble(id: PromptId, slots: &ResolvedSlots, instructions: &str) -> String {
    let mut out = id.template().to_string();
    for slot in Slot::ALL {
        out = fill_marker(&out, slot.marker(), &render_slot(slots, id, *slot));
    }
    out.replace(INSTRUCTIONS_MARKER, instructions)
}

pub fn assemble_raw(template: &str, slots: &ResolvedSlots, instructions: &str) -> String {
    let mut out = template.to_string();
    for slot in Slot::ALL {
        out = fill_marker(
            &out,
            slot.marker(),
            &render_slot(slots, PromptId::System, *slot),
        );
    }
    out.replace(INSTRUCTIONS_MARKER, instructions)
}

fn fill_marker(template: &str, marker: &str, content: &str) -> String {
    if content.is_empty() {
        return template
            .replace(&format!("{marker}\n"), "")
            .replace(marker, "");
    }
    template.replace(marker, content)
}

/// Minimal `{key}` substitution for env-style template variables (`{cwd}`,
/// `{platform}`, `{date}`). Mirrors the reference's `template::Vars` surface.
#[derive(Default)]
pub struct Vars(Vec<(String, String)>);

impl Vars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.0.push((key.to_owned(), value.into()));
        self
    }

    pub fn apply<'a>(&self, template: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(template);
        for (key, value) in &self.0 {
            if out.contains(key.as_str()) {
                match &mut out {
                    Cow::Borrowed(text) => {
                        out = Cow::Owned(text.replace(key.as_str(), value));
                    }
                    Cow::Owned(text) => *text = text.replace(key.as_str(), value),
                }
            }
        }
        out
    }
}

/// Today's UTC date as `YYYY-MM-DD` (civil-from-days, Hinnant's algorithm).
pub fn today_utc() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch to a proleptic Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Assemble the root agent's system prompt: the env section (cwd, platform,
/// date) is prepended to `instructions` (the user preamble followed by any
/// discovered AGENTS.md instruction text, C.15) and substituted into the
/// system template's `{{instructions}}` slot.
pub fn build_system_prompt(vars: &Vars, instructions: &str, slots: &ResolvedSlots) -> String {
    let env = vars.apply(
        "\n\nEnvironment:\n- Working directory: {cwd}\n- Platform: {platform}\n- Date: {date}",
    );
    let instructions = format!("{env}{instructions}");
    assemble_raw(SYSTEM_PROMPT, slots, &instructions)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn empty_slots_emit_defaults_and_no_markers() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Craft"));
        assert!(
            !out.contains("{{"),
            "unfilled marker left in output:\n{out}"
        );
        // No efficient tools exist yet; the line must be omitted entirely.
        assert!(!out.contains("Most efficient tools"));
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
    fn efficient_tools_extras_render_the_line() {
        let s = slots(
            PromptId::System,
            &[
                (Slot::EfficientTools, "outline"),
                (Slot::EfficientTools, "foo"),
            ],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Most efficient tools: outline, foo."));
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
        for pid in PromptId::ALL {
            s.insert(
                *pid,
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
        assert!(out.contains("Most efficient tools: EXTRA."));
    }

    #[test]
    fn has_slot_matrix() {
        let system_all = [
            Slot::ToolUsage,
            Slot::EfficientTools,
            Slot::Conventions,
            Slot::AfterInstructions,
            Slot::SubagentBriefing,
            Slot::Identity,
            Slot::Tone,
        ];
        for slot in system_all {
            assert!(PromptId::System.has_slot(slot), "system missing {slot}");
        }
        for slot in [
            Slot::Conventions,
            Slot::AfterInstructions,
            Slot::Identity,
            Slot::Tone,
        ] {
            assert!(!PromptId::Research.has_slot(slot), "research has {slot}");
        }
        for slot in [Slot::AfterInstructions, Slot::Identity, Slot::Tone] {
            assert!(!PromptId::General.has_slot(slot), "general has {slot}");
        }
        assert!(PromptId::General.has_slot(Slot::Conventions));
    }

    #[test]
    fn parse_is_the_plugin_contract() {
        assert_eq!(
            "after_instructions".parse::<Slot>().ok(),
            Some(Slot::AfterInstructions)
        );
        assert_eq!("identity".parse::<Slot>().ok(), Some(Slot::Identity));
        assert_eq!("tool_usagee".parse::<Slot>().ok(), None);
        assert_eq!("system".parse::<PromptId>().ok(), Some(PromptId::System));
        assert_eq!("systm".parse::<PromptId>().ok(), None);
        assert_eq!(Slot::Conventions.to_string(), "conventions");
        assert_eq!(PromptId::Research.to_string(), "research");
    }

    #[test]
    fn singleton_default_used_when_empty() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(out.starts_with("You are Craft"));
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
        for (plugin, content) in [("first", "FIRST"), ("second", "SECOND")] {
            s.insert(
                PromptId::System,
                Slot::Identity,
                SlotEntry {
                    plugin: Arc::from(plugin),
                    content: content.into(),
                },
            );
        }
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("SECOND"));
        assert!(!out.contains("FIRST"));
    }

    #[test]
    fn subagent_briefing_default_renders_and_is_overridable() {
        let out = assemble(PromptId::System, &ResolvedSlots::default(), "");
        assert!(
            out.contains("# Delegating to subagents"),
            "default briefing missing from system prompt:\n{out}"
        );

        let s = slots(
            PromptId::System,
            &[(Slot::SubagentBriefing, "- CUSTOM_BRIEFING")],
        );
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("- CUSTOM_BRIEFING"));
        assert!(!out.contains("# Delegating to subagents"));
    }

    #[test]
    fn conventions_entry_appends_to_template_defaults() {
        let s = slots(PromptId::System, &[(Slot::Conventions, "- Extra rule")]);
        let out = assemble(PromptId::System, &s, "");
        assert!(out.contains("Never assume a library is available"));
        assert!(out.contains("- Extra rule"));
    }

    #[test]
    fn build_system_prompt_prepends_env_section() {
        let vars = Vars::new()
            .set("{cwd}", "/tmp/proj")
            .set("{platform}", "macos")
            .set("{date}", "2026-09-13");
        let out = build_system_prompt(&vars, "EXTRA_INSTR", &ResolvedSlots::default());
        let env = at(
            &out,
            "\n\nEnvironment:\n- Working directory: /tmp/proj\n- Platform: macos\n- Date: 2026-09-13",
        );
        assert!(
            env < at(&out, "EXTRA_INSTR"),
            "instructions must follow env"
        );
        assert!(out.starts_with("You are Craft"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn vars_apply_substitutes_only_known_keys() {
        let vars = Vars::new().set("{cwd}", "/x");
        assert_eq!(vars.apply("dir {cwd} ({platform})"), "dir /x ({platform})");
        assert_eq!(vars.apply("no keys"), "no keys");
    }

    #[test]
    fn compaction_prompts_are_loaded() {
        assert!(COMPACTION_SYSTEM.contains("summarizing conversations"));
        assert!(COMPACTION_USER.contains("## Goal"));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_635), (2026, 7, 1));
    }
}
