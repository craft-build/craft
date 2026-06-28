//! Time-traveling stream rules (TTSR): dormant rules that abort an in-flight
//! model stream the moment emitted text matches, inject the rule as a system
//! reminder, and retry the turn. Course-correction with no context tax on every
//! turn, because the rules are only paid for when they fire.
//!
//! Distinct from `guardrails.rs` (which counts repeated tool failures) and
//! `doom.rs` (which watches for stagnation). TTSR watches the stream *content*
//! against a rule set and interrupts before a bad turn completes.
//!
//! Off by default (`ttsr.enabled`). Rules are regex patterns loaded from
//! `.craft/rules/*.md` (one regex per line prefixed with `rule:`, e.g.
//! `rule: Box::leak`). The repeat policy (`once` | `after-gap:N`) controls how
//! often a rule re-fires across turns within a session.

use std::collections::HashMap;
use std::sync::Mutex;

use regex::Regex;
use tracing::{info, warn};

use crate::discovery::Discovery;

const RULE_PREFIX: &str = "rule:";
const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatPolicy {
    #[default]
    Once,
    AfterGap(u32),
}

#[derive(Debug)]
pub struct TtsrRule {
    pub pattern: String,
    pub regex: Regex,
    pub message: String,
    pub repeat: RepeatPolicy,
}

#[derive(Debug, Default)]
struct RuleFired {
    /// Turn index of the last fire, or `None` when never fired.
    last_turn: Option<u32>,
}

/// Per-session TTSR state: loaded rules + per-rule firing memory + the running
/// text buffer for the current turn. Reset on compaction/session switch.
#[derive(Debug)]
pub struct TtsrManager {
    rules: Vec<TtsrRule>,
    fired: Mutex<HashMap<String, RuleFired>>,
    buffer: Mutex<String>,
    enabled: bool,
}

impl TtsrManager {
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            rules: Vec::new(),
            fired: Mutex::new(HashMap::new()),
            buffer: Mutex::new(String::new()),
            enabled: false,
        }
    }

    /// Load rules from `.craft/rules/*.md` (and global config). Invalid regex
    /// patterns are skipped with a warning. A missing/empty rule set leaves TTSR
    /// dormant even when enabled.
    pub fn load_from_discovery() -> Self {
        let mut rules = Vec::new();
        for file in Discovery::from_env().discover_files("rules", &["md"]) {
            for line in file.content.lines() {
                if let Some(rule) = parse_rule(line) {
                    rules.push(rule);
                }
            }
        }
        info!(rules = rules.len(), "ttsr rules loaded");
        Self {
            rules,
            fired: Mutex::new(HashMap::new()),
            buffer: Mutex::new(String::new()),
            enabled: true,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled && !self.rules.is_empty()
    }

    /// Reset the per-turn text buffer at the start of each turn.
    pub fn reset_turn(&self) {
        self.buffer.lock().unwrap().clear();
    }

    /// Append a text/thinking delta to the running buffer and return any rule
    /// that fires against the accumulated text this turn (respecting the repeat
    /// policy). The caller aborts the stream and injects the returned rule.
    pub fn observe(&self, delta: &str, turn: u32) -> Option<&TtsrRule> {
        if !self.enabled {
            return None;
        }
        let fired_rule = {
            let mut buffer = self.buffer.lock().unwrap();
            buffer.push_str(delta);
            self.rules.iter().find(|rule| rule.regex.is_match(&buffer))
        };
        let rule = fired_rule?;
        if !self.should_fire(&rule.pattern, rule.repeat, turn) {
            return None;
        }
        self.record_fire(&rule.pattern, turn);
        info!(pattern = %rule.pattern, "ttsr rule fired; aborting stream");
        Some(rule)
    }

    fn should_fire(&self, pattern: &str, repeat: RepeatPolicy, turn: u32) -> bool {
        let fired = self.fired.lock().unwrap();
        let Some(prev) = fired.get(pattern) else {
            return true;
        };
        match repeat {
            RepeatPolicy::Once => false,
            RepeatPolicy::AfterGap(gap) => turn >= prev.last_turn.unwrap_or(0) + gap,
        }
    }

    fn record_fire(&self, pattern: &str, turn: u32) {
        self.fired.lock().unwrap().insert(
            pattern.to_string(),
            RuleFired {
                last_turn: Some(turn),
            },
        );
    }

    /// Build the injected system-reminder message for a fired rule.
    pub fn injection(rule: &TtsrRule) -> String {
        format!(
            "{SYSTEM_REMINDER_OPEN}\n{}\n{SYSTEM_REMINDER_CLOSE}",
            rule.message
        )
    }

    /// Reset all firing memory (e.g. on compaction, so suppressed rules can re-fire).
    pub fn reset(&self) {
        self.fired.lock().unwrap().clear();
        self.buffer.lock().unwrap().clear();
    }
}

fn parse_rule(line: &str) -> Option<TtsrRule> {
    let raw = line.trim();
    let body = raw.strip_prefix(RULE_PREFIX)?.trim();
    if body.is_empty() {
        return None;
    }
    let (pattern, message, repeat) = split_message(body);
    let regex = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => {
            warn!(pattern = %pattern, error = %e, "ttsr: skipping rule with invalid regex");
            return None;
        }
    };
    Some(TtsrRule {
        pattern,
        regex,
        message,
        repeat,
    })
}

fn split_message(body: &str) -> (String, String, RepeatPolicy) {
    // `pattern | "human message" | once|after-gap:N`
    let parts: Vec<&str> = body.splitn(3, '|').map(|s| s.trim()).collect();
    let pattern = parts[0].to_string();
    let message = parts
        .get(1)
        .map(|s| s.trim_matches(['"', '\'']).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Avoid emitting text matching: {pattern}"));
    let repeat = parts
        .get(2)
        .map(|s| parse_repeat(s.trim()))
        .unwrap_or(RepeatPolicy::Once);
    (pattern, message, repeat)
}

fn parse_repeat(s: &str) -> RepeatPolicy {
    if s == "once" {
        return RepeatPolicy::Once;
    }
    if let Some(rest) = s.strip_prefix("after-gap:")
        && let Ok(n) = rest.parse::<u32>()
        && n > 0
    {
        return RepeatPolicy::AfterGap(n);
    }
    RepeatPolicy::Once
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn mgr(rules: &[&str]) -> TtsrManager {
        let rules: Vec<TtsrRule> = rules.iter().map(|r| parse_rule(r).unwrap()).collect();
        TtsrManager {
            rules,
            fired: Mutex::new(HashMap::new()),
            buffer: Mutex::new(String::new()),
            enabled: true,
        }
    }

    #[test_case("rule: Box::leak", "Box::leak", "Avoid emitting text matching: Box::leak" ; "bare_pattern")]
    #[test_case("rule: TODO\\(| \"leave no todos\" ", "TODO\\(", "leave no todos" ; "with_message")]
    fn parse_rule_parts(line: &str, pattern: &str, message: &str) {
        let r = parse_rule(line).unwrap();
        assert_eq!(r.pattern, pattern);
        assert_eq!(r.message, message);
    }

    #[test_case("not a rule" ; "not_a_rule")]
    #[test_case("rule:" ; "empty_rule")]
    #[test_case("rule:   " ; "blank_rule")]
    fn parse_rule_ignores_non_rules(line: &str) {
        assert!(parse_rule(line).is_none());
    }

    #[test]
    fn parse_rule_skips_invalid_regex() {
        assert!(parse_rule("rule: [unclosed").is_none());
    }

    #[test_case("once", RepeatPolicy::Once ; "once_explicit")]
    #[test_case("after-gap:5", RepeatPolicy::AfterGap(5) ; "after_gap_5")]
    #[test_case("after-gap:0", RepeatPolicy::Once ; "after_gap_zero_falls_back")]
    #[test_case("garbage", RepeatPolicy::Once ; "garbage_falls_back")]
    fn parse_repeat_classifies(s: &str, expected: RepeatPolicy) {
        assert_eq!(parse_repeat(s), expected);
    }

    #[test]
    fn observe_fires_on_match_then_suppresses_once() {
        let m = mgr(&["rule: Box::leak"]);
        assert!(m.observe("let x = Box::leak", 1).is_some());
        // Same turn buffer already contains it; but Once means it won't re-fire
        // on subsequent turns.
        assert!(
            m.observe("more text", 2).is_none(),
            "Once rule should not re-fire"
        );
    }

    #[test]
    fn observe_after_gap_re_fires() {
        let m = mgr(&["rule: TODO\\( | \"no todos\" | after-gap:2"]);
        assert!(m.observe("TODO(", 1).is_some(), "first fire at turn 1");
        assert!(m.observe("TODO(", 2).is_none(), "gap not elapsed at turn 2");
        assert!(
            m.observe("TODO(", 3).is_some(),
            "re-fires at turn 3 (gap=2)"
        );
    }

    #[test]
    fn observe_accumulates_across_deltas() {
        let m = mgr(&["rule: secret_key"]);
        assert!(m.observe("hello sec", 1).is_none());
        assert!(
            m.observe("ret_key here", 1).is_some(),
            "match across deltas"
        );
    }

    #[test]
    fn observe_no_match_returns_none() {
        let m = mgr(&["rule: forbidden"]);
        assert!(m.observe("perfectly fine text", 1).is_none());
    }

    #[test]
    fn disabled_never_fires() {
        let mut m = mgr(&["rule: x"]);
        m.enabled = false;
        assert!(m.observe("x", 1).is_none());
    }

    #[test]
    fn injection_wraps_in_system_reminder() {
        let r = parse_rule("rule: bad | \"do not do this\"").unwrap();
        let inj = TtsrManager::injection(&r);
        assert!(inj.starts_with("<system-reminder>"));
        assert!(inj.ends_with("</system-reminder>"));
        assert!(inj.contains("do not do this"));
    }

    #[test]
    fn reset_clears_firing_memory() {
        let m = mgr(&["rule: x"]);
        assert!(m.observe("x", 1).is_some());
        m.reset();
        assert!(m.observe("x", 2).is_some(), "reset allows re-fire");
    }

    #[test]
    fn reset_turn_clears_buffer() {
        let m = mgr(&["rule: abc"]);
        m.observe("ab", 1);
        m.reset_turn();
        assert!(m.observe("c", 1).is_none(), "buffer cleared so no match");
    }

    #[test]
    fn enabled_reflects_rules() {
        let m = TtsrManager::disabled();
        assert!(!m.enabled());
        let m = mgr(&["rule: x"]);
        assert!(m.enabled());
        let empty = TtsrManager {
            rules: Vec::new(),
            fired: Mutex::new(HashMap::new()),
            buffer: Mutex::new(String::new()),
            enabled: true,
        };
        assert!(!empty.enabled(), "enabled but no rules => dormant");
    }
}
