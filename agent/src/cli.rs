//! G.1 core CLI flags, ported from the reference `src/cli.rs`. The flag
//! surface (names, shorts, aliases, value enums) mirrors the reference
//! exactly; each flag's semantics land with its own subsystem. Print/SDK
//! flags are parsed and stored here for the full headless mode (task 84).

use std::io::IsTerminal;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};

use crate::error::{InvalidSnafu, Result};
use crate::id::SessionRef;
use crate::permissions::{
    Effect, PermissionRule, ToolKey, is_valid_server_name, is_valid_wire_name,
};
use crate::run::BeforeExecute;
use crate::storage::StateDir;

#[derive(Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum PromptVariant {
    #[default]
    System,
    Research,
    General,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum CliMode {
    #[default]
    Build,
    Plan,
    /// Parsed for compatibility, rejected by [`Cli::validate`] until Flow
    /// mode is ported (task 99).
    Flow,
}

/// The resolved permission posture for a run: standard (fail closed on
/// unresolved asks headlessly), `--yolo` (bypass every check, including
/// deny rules), or `-A/--auto-review` (a reviewer answers asks). A single
/// choice: the two bypasses are mutually exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionPolicy {
    #[default]
    Standard,
    Yolo,
    AutoReview,
}

impl PermissionPolicy {
    /// Map the parsed flags, rejecting the impossible combination up front
    /// so every surface (print, term, recipe, TUI) enforces the same rule.
    /// `--permission-mode bypassPermissions` is the same posture as
    /// `--yolo`, so it feeds the same exclusivity check.
    pub fn from_flags(yolo: bool, auto_review: bool) -> Result<Self> {
        match (yolo, auto_review) {
            (true, true) => InvalidSnafu {
                reason: "--yolo and --auto-review are mutually exclusive".to_string(),
            }
            .fail(),
            (true, false) => Ok(Self::Yolo),
            (false, true) => Ok(Self::AutoReview),
            (false, false) => Ok(Self::Standard),
        }
    }
}

/// `--permission-mode` (Claude Code SDK spellings preserved). `default` is
/// the standard posture; `bypassPermissions` matches `--yolo`; `plan` maps
/// to plan mode; `acceptEdits` parses but is rejected by [`Cli::validate`]
/// until it is implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum CliPermissionMode {
    #[default]
    #[value(name = "default")]
    Default,
    #[value(name = "acceptEdits")]
    AcceptEdits,
    #[value(name = "bypassPermissions")]
    BypassPermissions,
    #[value(name = "plan")]
    Plan,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Craft coding agent: launches the interactive TUI by default",
    long_about = "Craft coding agent. With no subcommand, launches the interactive terminal UI \
                  backed by the configured providers in ~/.config/craft.bml. With \
                  --print, runs one prompt headlessly and exits."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Non-interactive mode. Runs the prompt and exits
    /// (Claude Code's --print flag).
    #[arg(short, long)]
    pub print: bool,

    /// Attach an image to the prompt in --print mode as vision content
    /// (repeatable; task 84).
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<std::path::PathBuf>,

    /// Model spec (provider/model-id). Defaults to the medium-tier model.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Include full turn-by-turn messages in --print output (task 84).
    #[arg(long)]
    pub verbose: bool,

    /// Resume the most recent session in this directory (TUI and --print;
    /// F.3 resume-latest-by-cwd). With no prior session, --print starts a
    /// fresh one after a stderr warning.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(short = 's', long, alias = "resume")]
    pub session: Option<String>,

    /// Output format for --print mode (task 84).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Initial mode (build, plan, flow). Defaults to build; kept optional
    /// so an explicit `--mode` stays distinguishable from the default when
    /// `--permission-mode plan` asks for plan mode.
    #[arg(long, value_enum)]
    pub mode: Option<CliMode>,

    /// Input format (text or stream-json for SDK mode; task 84).
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Skip discovering custom slash commands (`.craft/commands`,
    /// `.claude/commands`) in the interactive TUI.
    #[arg(long)]
    pub no_commands: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long, alias = "dangerously-skip-permissions")]
    pub yolo: bool,

    /// Auto-decide permission prompts with an LLM reviewer instead of asking
    #[arg(short = 'A', long)]
    pub auto_review: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Pre-approve tools (comma-separated): adds scope-universal allow
    /// rules so the named tools run without a permission prompt. This does
    /// NOT restrict the advertised tool set — every tool stays available.
    /// Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Deny tools at the permission gate (comma-separated): calls to the
    /// named tools fail with a permission-denied message; they stay
    /// advertised. Accepts PascalCase or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Persistence ID for a NEW session (SDK compat); never resumes one.
    /// Combining it with --session/--continue requires --fork-session.
    #[arg(long)]
    pub session_id: Option<String>,

    /// Copy the resumed session (--session or --continue) under a new ID,
    /// leaving the source file unchanged (--print only).
    #[arg(long)]
    pub fork_session: bool,

    /// Maximum number of agent turns
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// System prompt override
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append to system prompt
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// Permission posture: `default`, `plan` (plan mode), or
    /// `bypassPermissions` (same as `--yolo`). `acceptEdits` is rejected
    /// as not supported yet.
    #[arg(long, value_enum)]
    pub permission_mode: Option<CliPermissionMode>,

    /// Include partial streaming messages in SDK output (task 84).
    #[arg(long)]
    pub include_partial_messages: bool,

    /// Permission prompt tool (accepted for compat, used in SDK mode).
    #[arg(long, hide = true)]
    pub permission_prompt_tool: Option<String>,

    // Accepted but ignored, so Claude Code SDK callers don't break.
    #[arg(long, hide = true)]
    pub fallback_model: Option<String>,
    #[arg(long, hide = true)]
    pub settings: Option<String>,
    #[arg(long, hide = true)]
    pub setting_sources: Option<String>,
    #[arg(long, hide = true)]
    pub add_dir: Option<String>,
    #[arg(long, hide = true)]
    pub strict_mcp_config: bool,
    #[arg(long, hide = true)]
    pub include_hook_events: bool,
    #[arg(long, hide = true)]
    pub mcp_config: Option<String>,
    #[arg(long, hide = true)]
    pub tools: Option<String>,
    #[arg(long, hide = true)]
    pub betas: Option<String>,
    /// Thinking-token budget where supported; maps to effort on effort-only APIs.
    #[arg(long, conflicts_with_all = ["thinking", "effort"])]
    pub max_thinking_tokens: Option<String>,
    /// Reasoning effort: minimal, low, medium, high, xhigh, or max.
    #[arg(long, conflicts_with_all = ["thinking", "max_thinking_tokens"])]
    pub effort: Option<String>,
    #[arg(long, hide = true)]
    pub json_schema: Option<String>,
    #[arg(long, hide = true)]
    pub max_budget_usd: Option<String>,
    /// Thinking policy: off, adaptive, an effort level, or a positive token budget.
    #[arg(long, conflicts_with_all = ["effort", "max_thinking_tokens"])]
    pub thinking: Option<String>,
    #[arg(long, hide = true)]
    pub thinking_display: Option<String>,

    /// Initial prompt (reads stdin if piped)
    #[arg(value_name = "PROMPT")]
    pub initial_prompt: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    /// Serve the agent loop over the ACP protocol on stdin/stdout (for
    /// editors and other ACP clients).
    Acp,
    /// Emit shell completion scripts for the given shell to stdout.
    Completions { shell: clap_complete::Shell },
    /// List every model available from the configured providers.
    Models,
    /// Show cost and usage stats from the persistent ledger.
    Stats {
        /// Show per-session breakdown instead of the default per-model view.
        #[arg(long)]
        sessions: bool,
    },
    /// Diagnose provider configuration and self-heal to a working provider.
    Doctor {
        /// Export a JSON diagnostics report instead of running self-heal.
        #[arg(long)]
        export: bool,
    },
    /// Update craft to the latest release (digest-pinned install script).
    Update {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Disable script highlighting (accepted; the script always
        /// prints plainly here)
        #[arg(long)]
        no_color: bool,
    },
    /// Rollback to the version saved by the last update
    Rollback,
    /// Show the rendered system prompt or tool definitions.
    Prompt {
        /// Prompt variant: system (default), research, general.
        #[arg(value_enum, default_value_t = PromptVariant::System)]
        variant: PromptVariant,
        /// Append the plan mode reminder to the system prompt.
        #[arg(long)]
        plan: bool,
        /// Show tool definitions (JSON) instead of prompt text.
        #[arg(long)]
        tools: bool,
        /// With --tools: show only tool names, one per line.
        #[arg(long, requires = "tools")]
        names: bool,
    },
    /// Shell integration: init scripts, command logging, `@craft` queries.
    Term {
        #[command(subcommand)]
        action: TermAction,
    },
    /// Browse and run parameterized recipes (J.5).
    Recipe {
        #[command(subcommand)]
        action: RecipeAction,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum RecipeAction {
    /// List discovered recipes as `name<TAB>description`.
    List,
    /// Run a recipe: resolve parameters, render its template, and run the
    /// result as a headless query.
    Run {
        /// Recipe name (file stem or the recipe's own `name` field).
        name: String,
        /// Recipe parameter override, `key=value` (repeatable).
        #[arg(short = 'p', long = "param")]
        param: Vec<String>,
        /// Model spec (provider/model-id); the recipe's `model` field wins.
        #[arg(short = 'm', long)]
        model: Option<String>,
        /// Output format for the result.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output_format: OutputFormat,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum TermAction {
    /// Print a shell init script that logs every command and defines `@craft`
    Init {
        /// Target shell
        shell: ShellKind,
        /// Also install a command_not_found handler that asks craft on miss
        #[arg(long)]
        with_not_found: bool,
    },
    /// Append a shell command to the current directory's command history
    Log {
        /// The command that was run
        command: String,
    },
    /// Run a headless agent query with recent shell history injected as context
    Run {
        /// The query for craft
        query: Vec<String>,
        /// Model spec (provider/model-id)
        #[arg(short = 'm', long)]
        model: Option<String>,
        /// Output format for the result
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output_format: OutputFormat,
    },
    /// Show the active session id and recent logged commands for this directory
    Info,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl Cli {
    pub fn warn_ignored_flags(&self) {
        let ignored = [
            ("fallback-model", self.fallback_model.is_some()),
            ("settings", self.settings.is_some()),
            ("setting-sources", self.setting_sources.is_some()),
            ("add-dir", self.add_dir.is_some()),
            ("strict-mcp-config", self.strict_mcp_config),
            ("include-hook-events", self.include_hook_events),
            ("mcp-config", self.mcp_config.is_some()),
            ("tools", self.tools.is_some()),
            ("betas", self.betas.is_some()),
            ("json-schema", self.json_schema.is_some()),
            ("max-budget-usd", self.max_budget_usd.is_some()),
            ("thinking-display", self.thinking_display.is_some()),
        ];
        for (flag, set) in &ignored {
            if *set {
                eprintln!("warning: --{flag} is accepted but ignored");
            }
        }
    }

    pub fn is_sdk_mode(&self) -> bool {
        self.print && matches!(self.input_format, InputFormat::StreamJson)
    }

    /// The run's permission policy from `--yolo` / `-A` (with
    /// `--permission-mode bypassPermissions` counting as `--yolo`); the
    /// exclusive pair is rejected here (via [`Cli::validate`]) for every
    /// surface.
    pub fn permission_policy(&self) -> Result<PermissionPolicy> {
        let yolo = self.yolo || self.permission_mode == Some(CliPermissionMode::BypassPermissions);
        PermissionPolicy::from_flags(yolo, self.auto_review)
    }

    /// The effective initial mode: an explicit `--mode` wins, else
    /// `--permission-mode plan` upgrades the default to plan mode.
    pub fn run_mode(&self) -> CliMode {
        match (&self.permission_mode, self.mode.as_ref()) {
            (Some(CliPermissionMode::Plan), None) => CliMode::Plan,
            (_, Some(mode)) => mode.clone(),
            _ => CliMode::Build,
        }
    }

    /// Normalize one `--allowed-tools` / `--disallowed-tools` entry to a
    /// [`ToolKey`]. PascalCase native names (`Read`) become their snake
    /// registry names (`read`); snake names pass through. MCP tools accept
    /// the wire form `server__tool`, the Claude-Code spellings
    /// `mcp__server__tool` / `mcp__server` (server-wide); MCP entries are
    /// syntax-validated only, since they cannot be checked against a fixed
    /// registry.
    pub fn normalize_tool_spec(spec: &str) -> Result<ToolKey> {
        let spec = spec.trim();
        if spec.is_empty() {
            return InvalidSnafu {
                reason: "empty tool name in --allowed-tools/--disallowed-tools".to_string(),
            }
            .fail();
        }
        if spec == "*" {
            return InvalidSnafu {
                reason: "wildcard tool rules are not supported on the CLI; \
                         use a `*` rule in permissions.bml instead"
                    .to_string(),
            }
            .fail();
        }
        let body = spec.strip_prefix("mcp__").unwrap_or(spec);
        if body != spec || body.contains("__") {
            return match body.split_once("__") {
                Some((server, tool))
                    if is_valid_server_name(server) && is_valid_wire_name(tool) =>
                {
                    Ok(ToolKey::McpTool {
                        server: server.into(),
                        tool: tool.into(),
                    })
                }
                None if is_valid_server_name(body) => Ok(ToolKey::McpServer {
                    server: body.into(),
                }),
                _ => InvalidSnafu {
                    reason: format!(
                        "invalid MCP tool spec {spec:?}: expected mcp__server__tool, \
                         mcp__server, or the wire form server__tool"
                    ),
                }
                .fail(),
            };
        }
        let normalized = pascal_to_snake(spec);
        let names = crate::tools::native_tool_names();
        // Some registry names are concatenated (`webfetch`, `multiedit`),
        // so `WebFetch`/`web_fetch` both resolve after the snake pass.
        let hit = if names.contains(&normalized.as_str()) {
            normalized
        } else {
            let concatenated = normalized.replace('_', "");
            if names.contains(&concatenated.as_str()) {
                concatenated
            } else {
                return InvalidSnafu {
                    reason: format!("unknown tool '{spec}'. Valid tools: {}", names.join(", ")),
                }
                .fail();
            }
        };
        Ok(ToolKey::native(&hit))
    }

    /// The CLI tool policy as scope-universal permission rules: one `Allow`
    /// per `--allowed-tools` entry, one `Deny` per `--disallowed-tools`
    /// entry. Overlapping names are rejected by [`Cli::validate`].
    pub fn tool_policy(&self) -> Result<Vec<PermissionRule>> {
        let mut allow = Vec::new();
        for spec in &self.allowed_tools {
            let key = Self::normalize_tool_spec(spec)?;
            if !allow.contains(&key) {
                allow.push(key);
            }
        }
        let mut deny = Vec::new();
        for spec in &self.disallowed_tools {
            let key = Self::normalize_tool_spec(spec)?;
            if !deny.contains(&key) {
                deny.push(key);
            }
        }
        if let Some(conflict) = allow.iter().find(|key| deny.contains(key)) {
            return InvalidSnafu {
                reason: format!(
                    "--allowed-tools and --disallowed-tools both name `{conflict}`; pick one"
                ),
            }
            .fail();
        }
        let rules = allow
            .into_iter()
            .map(|tool| PermissionRule {
                tool,
                scope: None,
                effect: Effect::Allow,
            })
            .chain(deny.into_iter().map(|tool| PermissionRule {
                tool,
                scope: None,
                effect: Effect::Deny,
            }))
            .collect();
        Ok(rules)
    }

    /// Cross-flag validation the reference performs before dispatch.
    /// Parse-level mistakes (bad enum values, unknown flags) are already
    /// handled by clap.
    pub fn validate(&self) -> Result<()> {
        if let Some(mode) = self.permission_mode {
            match mode {
                CliPermissionMode::Default => {}
                CliPermissionMode::AcceptEdits => {
                    return InvalidSnafu {
                        reason: "--permission-mode acceptEdits is not supported yet".to_string(),
                    }
                    .fail();
                }
                CliPermissionMode::BypassPermissions => {
                    if self.auto_review {
                        return InvalidSnafu {
                            reason: "--permission-mode bypassPermissions cannot be combined \
                                     with -A/--auto-review"
                                .to_string(),
                        }
                        .fail();
                    }
                }
                CliPermissionMode::Plan => {
                    if self
                        .mode
                        .as_ref()
                        .is_some_and(|mode| mode != &CliMode::Plan)
                    {
                        return InvalidSnafu {
                            reason: "--permission-mode plan cannot be combined with \
                                     an explicit --mode build"
                                .to_string(),
                        }
                        .fail();
                    }
                }
            }
        }
        self.permission_policy()?;
        self.tool_policy()?;
        self.thinking_override()?;
        if matches!(self.run_mode(), CliMode::Flow) {
            return InvalidSnafu {
                reason: "--mode flow is not available yet (Flow mode is ported last)",
            }
            .fail();
        }
        if self.print {
            if matches!(self.input_format, InputFormat::StreamJson) {
                return InvalidSnafu {
                    reason: "--input-format stream-json (SDK mode) is not supported yet",
                }
                .fail();
            }
        } else {
            let print_only = [
                ("--verbose", self.verbose),
                ("--output-format", self.output_format != OutputFormat::Text),
                ("--input-format", self.input_format != InputFormat::Text),
                ("--image", !self.images.is_empty()),
                ("--include-partial-messages", self.include_partial_messages),
                ("--fork-session", self.fork_session),
                ("--exit-on-done", self.exit_on_done),
            ];
            for (flag, set) in &print_only {
                if *set {
                    return InvalidSnafu {
                        reason: format!("{flag} requires --print"),
                    }
                    .fail();
                }
            }
        }
        if self.fork_session && self.session.is_none() && !self.continue_session {
            return InvalidSnafu {
                reason: "--fork-session requires --session or --continue",
            }
            .fail();
        }
        if self.session_id.is_some()
            && (self.session.is_some() || self.continue_session)
            && !self.fork_session
        {
            return InvalidSnafu {
                reason: "--session-id names a new session and never resumes; \
                         combine it with --session/--continue only via --fork-session",
            }
            .fail();
        }
        if self.continue_session && self.session.is_some() {
            return InvalidSnafu {
                reason: "--continue and --session are mutually exclusive",
            }
            .fail();
        }
        Ok(())
    }

    /// Resolve the three mutually exclusive reasoning flags into one policy.
    pub fn thinking_override(&self) -> Result<Option<crate::thinking::ThinkingConfig>> {
        use crate::thinking::ThinkingConfig;
        if [&self.thinking, &self.effort, &self.max_thinking_tokens]
            .iter()
            .filter(|value| value.is_some())
            .count()
            > 1
        {
            return InvalidSnafu {
                reason: "--thinking, --effort, and --max-thinking-tokens are mutually exclusive",
            }
            .fail();
        }
        let parsed = if let Some(value) = &self.thinking {
            Some(ThinkingConfig::parse(value, ThinkingConfig::default()))
        } else if let Some(value) = &self.effort {
            Some(
                ThinkingConfig::parse(value, ThinkingConfig::default()).and_then(|thinking| {
                    if matches!(thinking, ThinkingConfig::Effort(_)) {
                        Ok(thinking)
                    } else {
                        Err("expected effort: minimal, low, medium, high, xhigh, or max".to_owned())
                    }
                }),
            )
        } else {
            self.max_thinking_tokens.as_ref().map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| "expected a positive thinking token budget".to_owned())
                    .and_then(|budget| {
                        if budget == 0 {
                            Err("expected a positive thinking token budget".to_owned())
                        } else {
                            Ok(ThinkingConfig::Budget(budget))
                        }
                    })
            })
        };
        parsed
            .transpose()
            .map_err(|reason| InvalidSnafu { reason }.build())
    }

    /// Apply CLI policy before dispatching to any interactive or headless surface.
    pub fn apply_thinking(&self, config: &mut crate::config::Config) -> Result<()> {
        if let Some(thinking) = self.thinking_override()? {
            config.always_thinking = Some(thinking);
        }
        Ok(())
    }

    /// The system-prompt text that replaces the config preamble:
    /// `--system-prompt` replaces it, `--append-system-prompt` appends to it,
    /// otherwise the config value passes through unchanged.
    pub fn effective_preamble(&self, config_preamble: &str) -> String {
        let base = self
            .system_prompt
            .clone()
            .unwrap_or_else(|| config_preamble.to_string());
        match &self.append_system_prompt {
            Some(extra) if extra.trim().is_empty() => base,
            Some(extra) if base.trim().is_empty() => extra.clone(),
            Some(extra) => format!("{base}\n\n{extra}"),
            None => base,
        }
    }

    /// The session to resume, from `-s/--session` (alias `--resume`).
    /// `--session-id` and `--continue` are resolved separately by
    /// [`Self::resolve_session_plan`].
    pub fn resume_session(&self) -> Result<Option<SessionRef>> {
        match &self.session {
            Some(raw) => parse_session_ref(raw).map(Some),
            None => Ok(None),
        }
    }

    /// Resolve `--session` / `--continue` / `--session-id` /
    /// `--fork-session` into what a print-mode run should do (H.9).
    /// Malformed ids and missing/corrupt records on an explicit resume or
    /// fork are hard errors before any model work; `--continue` with no
    /// session for the cwd starts fresh with a stderr warning (TUI parity).
    pub fn resolve_session_plan(&self, dir: &StateDir, cwd: &str) -> Result<ResumePlan> {
        let source = self.resume_source(dir, cwd)?;
        if self.fork_session {
            let source = source.ok_or_else(|| {
                InvalidSnafu {
                    reason: format!(
                        "--fork-session has no source: no previous session in {cwd} \
                         (--continue found nothing)"
                    ),
                }
                .build()
            })?;
            return self.fork_source(dir, &source.id.clone());
        }
        if let Some(session) = source {
            return Ok(ResumePlan::Resume {
                id: session.id.clone(),
                model: stored_model_spec(&session),
            });
        }
        if self.continue_session {
            eprintln!("warning: no previous session in {cwd}; starting a new one");
        }
        // `--session-id` alone names the new session (SDK compat).
        Ok(ResumePlan::Fresh {
            id: self
                .session_id
                .as_deref()
                .map(parse_session_ref)
                .transpose()?,
        })
    }

    /// The session a resume/fork loads: the explicit `--session` target, or
    /// the `--continue` latest for `cwd`. `Ok(None)` means genuinely no
    /// session; an unreadable newest record is a hard error, not a silent
    /// downgrade ("fail fast with actionable errors", H.9).
    fn resume_source(
        &self,
        dir: &StateDir,
        cwd: &str,
    ) -> Result<Option<crate::headless::StoredSession>> {
        if let Some(raw) = &self.session {
            let id = parse_session_ref(raw)?;
            return load_or_invalid(raw, id.id(), dir).map(Some);
        }
        if !self.continue_session {
            return Ok(None);
        }
        // The cwd index names the newest session for `cwd`. When the record
        // it names is unreadable, `latest` degrades to an older sibling
        // (warn → rescan → silently resume it); an explicit `--continue`
        // must hard-error on the corrupt newest record instead. A stale
        // entry for a deleted session just falls through to the scan.
        if let Some(raw) = crate::storage::sessions::indexed_latest(cwd, dir)
            && let Ok(id) = raw.parse::<SessionRef>()
        {
            match crate::headless::StoredSession::load(id.id(), dir) {
                Ok(session) => return Ok(Some(session)),
                // A record deleted behind our back (stale index) is not
                // corruption; fall through to the scan. A file that exists
                // on disk but yields no readable record is a hard error.
                Err(crate::storage::sessions::SessionError::Storage {
                    source: crate::storage::StorageError::NotFound { .. },
                }) if !session_file_exists(dir, id.id()) => {}
                Err(e) => {
                    return Err(InvalidSnafu {
                        reason: format!(
                            "the latest session for {cwd} ({raw}) could not be loaded: {e}"
                        ),
                    }
                    .build());
                }
            }
        }
        crate::headless::StoredSession::latest(cwd, dir).map_err(|e| {
            InvalidSnafu {
                reason: format!("finding the latest session for {cwd}: {e}"),
            }
            .build()
        })
    }

    /// Copy the source session under a fresh (or `--session-id`) id and
    /// hand the run the copy; the source file is never modified.
    fn fork_source(&self, dir: &StateDir, source_id: &SessionRef) -> Result<ResumePlan> {
        let source = load_or_invalid(source_id.as_str(), source_id.id(), dir)?;
        let new_id = match &self.session_id {
            Some(raw) => parse_session_ref(raw)?,
            None => SessionRef::generate(),
        };
        let model = stored_model_spec(&source);
        let mut forked = source;
        forked.id = new_id.clone();
        forked.meta.input_draft = None;
        forked.save(dir).map_err(|e| {
            InvalidSnafu {
                reason: format!("forking session {source_id} under {new_id}: {e}"),
            }
            .build()
        })?;
        Ok(ResumePlan::Fork { new_id, model })
    }
}

/// What a print-mode run should do with sessions (H.9): start fresh
/// (optionally under a caller-supplied id), resume a stored session, or run
/// on a copy that was already written under a new id.
#[derive(Debug, PartialEq)]
pub enum ResumePlan {
    /// Start a new session; `--session-id X` names it, else one is generated.
    Fresh { id: Option<SessionRef> },
    /// Continue this stored session; its history loads from the store.
    Resume {
        id: SessionRef,
        model: Option<String>,
    },
    /// The source was already copied under `new_id`; the run continues the copy.
    Fork {
        new_id: SessionRef,
        model: Option<String>,
    },
}

/// Parse a session id with the shared, actionable error shape (H.9).
fn parse_session_ref(raw: &str) -> Result<SessionRef> {
    raw.parse::<SessionRef>().map_err(|_| {
        InvalidSnafu {
            reason: format!(
                "{raw:?} is not a valid session id \
                 (expected the id shown by `craft --continue` or /sessions)"
            ),
        }
        .build()
    })
}

/// Load a stored session or turn the storage error into an actionable CLI
/// error naming the requested id — explicit resume intent never degrades to
/// an unpersisted run (H.9).
fn load_or_invalid(
    raw: &str,
    id: crate::id::CraftId,
    dir: &StateDir,
) -> Result<crate::headless::StoredSession> {
    crate::headless::StoredSession::load(id, dir).map_err(|e| {
        InvalidSnafu {
            reason: format!("session {raw} could not be loaded: {e}"),
        }
        .build()
    })
}

/// Whether the session's JSONL file exists on disk — distinguishes a
/// genuinely deleted record (stale index) from one that exists but has no
/// readable header (both load as `NotFound`).
fn session_file_exists(dir: &StateDir, id: crate::id::CraftId) -> bool {
    dir.ensure_subdir(crate::storage::sessions::SESSIONS_DIR)
        .ok()
        .map(|sessions| sessions.join(format!("{id}.jsonl")).exists())
        .unwrap_or(true)
}

/// The stored `provider/model` a resume/fork restores when `--model` is
/// absent; `unknown` and legacy records without a spec carry no model.
fn stored_model_spec(session: &crate::headless::StoredSession) -> Option<String> {
    let model = session.model.trim();
    (!model.is_empty() && model != "unknown" && model.contains('/')).then(|| model.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("craft").chain(args.iter().copied()))
    }

    /// The positional prompt wins over piped stdin; piped stdin is read and
    /// trimmed; whitespace-only input collapses to `None` so the TUI starts
    /// normally; an attached terminal yields `None`.
    #[test]
    fn resolve_prompt_input_precedence_and_piping() {
        // Positional beats piped stdin.
        let piped = &mut "from stdin\n".as_bytes();
        assert_eq!(
            resolve_prompt_input(Some("from cli".into()), piped, false).unwrap(),
            Some("from cli".to_string())
        );
        // Piped stdin is the prompt when no positional was given.
        assert_eq!(
            resolve_prompt_input(None, &mut "  piped prompt \n".as_bytes(), false).unwrap(),
            Some("piped prompt".to_string())
        );
        // Whitespace-only piped input starts the TUI normally.
        assert_eq!(
            resolve_prompt_input(None, &mut " \n\t".as_bytes(), false).unwrap(),
            None
        );
        // An attached (interactive) terminal is never read.
        assert_eq!(
            resolve_prompt_input(None, &mut "".as_bytes(), true).unwrap(),
            None
        );
        // Whitespace-only positional collapses the same way.
        assert_eq!(
            resolve_prompt_input(Some("   ".into()), &mut "".as_bytes(), true).unwrap(),
            None
        );
    }

    /// `--print` without a positional and without piped stdin still errors
    /// (the unchanged run_print contract).
    #[tokio::test]
    async fn run_print_without_any_prompt_errors() {
        use std::io::IsTerminal;
        let cli = parse(&["--print"]).unwrap();
        if std::io::stdin().is_terminal() {
            assert!(
                run_print(&cli, crate::config::Config::default())
                    .await
                    .is_err()
            );
        } else {
            // Piped stdin the test harness controls is not portable across
            // runners; the terminal-attached branch above pins the contract.
        }
    }

    #[test]
    fn thinking_flags_resolve_and_validate() {
        use crate::thinking::ThinkingConfig;
        for (args, expected) in [
            (vec!["--thinking", "off"], ThinkingConfig::Off),
            (vec!["--thinking", "adaptive"], ThinkingConfig::Adaptive),
            (
                vec!["--max-thinking-tokens", "4096"],
                ThinkingConfig::Budget(4096),
            ),
        ] {
            let cli = parse(&args).unwrap();
            assert_eq!(cli.thinking_override().unwrap(), Some(expected));
        }
        for value in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            let cli = parse(&["--effort", value]).unwrap();
            assert_eq!(cli.thinking_override().unwrap().unwrap().to_string(), value);
        }
        for args in [
            vec!["--thinking", "bogus"],
            vec!["--effort", "off"],
            vec!["--max-thinking-tokens", "0"],
            vec!["--max-thinking-tokens", "-1"],
            vec!["--max-thinking-tokens", "4294967296"],
        ] {
            assert!(
                parse(&args)
                    .map(|cli| cli.validate().is_err())
                    .unwrap_or(true)
            );
        }
    }

    #[test]
    fn thinking_flags_are_mutually_exclusive() {
        assert!(parse(&["--thinking", "off", "--effort", "high"]).is_err());
        assert!(parse(&["--thinking", "adaptive", "--max-thinking-tokens", "4096"]).is_err());
        assert!(parse(&["--effort", "high", "--max-thinking-tokens", "4096"]).is_err());
        assert_eq!(parse(&[]).unwrap().thinking_override().unwrap(), None);
    }

    #[test]
    fn term_actions_parse_with_reference_names() {
        let cli = parse(&["term", "init", "zsh", "--with-not-found"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Commands::Term {
                action: TermAction::Init {
                    shell: ShellKind::Zsh,
                    with_not_found: true
                }
            })
        ));
        let cli = parse(&["term", "log", "cargo build"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Commands::Term {
                action: TermAction::Log { command }
            }) if command == "cargo build"
        ));
        let cli = parse(&["term", "run", "-m", "anthropic/m", "fix", "it"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Commands::Term {
                action: TermAction::Run {
                    query,
                    model: Some(spec),
                    output_format: OutputFormat::Text
                }
            }) if query == &["fix", "it"] && spec == "anthropic/m"
        ));
        let cli = parse(&["term", "info"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Commands::Term {
                action: TermAction::Info
            })
        ));
    }

    #[test]
    fn recipe_actions_parse_with_reference_names() {
        let cli = parse(&["recipe", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Recipe {
                action: RecipeAction::List
            })
        ));
        let cli = parse(&[
            "recipe",
            "run",
            "audit",
            "--param",
            "focus=security",
            "-p",
            "depth=3",
            "-m",
            "anthropic/m",
        ])
        .unwrap();
        let Commands::Recipe {
            action: RecipeAction::Run {
                name, param, model, ..
            },
        } = cli.command.unwrap()
        else {
            panic!("expected recipe run");
        };
        assert_eq!(name, "audit");
        assert_eq!(param, vec!["focus=security", "depth=3"]);
        assert_eq!(model.as_deref(), Some("anthropic/m"));
    }

    #[test]
    fn inject_context_wraps_context_block() {
        let out = inject_context("do thing", &["hist1".into(), "hist2".into()]);
        assert!(out.starts_with("<context>\n"));
        assert!(out.contains("hist1\nhist2\n"));
        assert!(out.ends_with("</context>\n\ndo thing"));
    }

    #[test]
    fn inject_context_passthrough_when_empty() {
        assert_eq!(inject_context("do thing", &[]), "do thing");
    }

    #[test]
    fn defaults_match_the_reference() {
        let cli = parse(&["hi"]).unwrap();
        assert!(!cli.print);
        assert_eq!(cli.run_mode(), CliMode::Build);
        assert_eq!(cli.output_format, OutputFormat::Text);
        assert_eq!(cli.input_format, InputFormat::Text);
        assert!(cli.images.is_empty());
        assert_eq!(cli.permission_policy().unwrap(), PermissionPolicy::Standard);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn print_flags_parse_with_reference_names() {
        let cli = parse(&[
            "-p",
            "--verbose",
            "--image",
            "/a.png",
            "--image",
            "/b.png",
            "-m",
            "anthropic/claude-opus-4-6",
            "--output-format",
            "stream-json",
            "--max-turns",
            "5",
            "--system-prompt",
            "sys",
            "--append-system-prompt",
            "more",
            "hello",
        ])
        .unwrap();
        assert!(cli.print);
        assert!(cli.verbose);
        assert_eq!(cli.images.len(), 2);
        assert_eq!(cli.model.as_deref(), Some("anthropic/claude-opus-4-6"));
        assert_eq!(cli.output_format, OutputFormat::StreamJson);
        assert_eq!(cli.max_turns, Some(5));
        assert_eq!(cli.initial_prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn yolo_alias_and_tool_flag_aliases_parse() {
        let cli = parse(&[
            "--dangerously-skip-permissions",
            "--allowedTools",
            "Read,Grep",
            "--disallowed-tools",
            "bash",
            "-A",
        ])
        .unwrap();
        assert!(cli.yolo);
        assert!(cli.auto_review);
        assert_eq!(cli.allowed_tools, ["Read", "Grep"]);
        assert_eq!(cli.disallowed_tools, ["bash"]);
        // Parsing accepts both; the pair is rejected by validate() (and by
        // the policy mapping), never by the flag surface itself.
        assert!(cli.permission_policy().is_err());
    }

    #[test]
    fn permission_policy_maps_flags_and_rejects_the_pair() {
        assert_eq!(
            parse(&["-p", "hi"]).unwrap().permission_policy().unwrap(),
            PermissionPolicy::Standard
        );
        assert_eq!(
            parse(&["-p", "--yolo", "hi"])
                .unwrap()
                .permission_policy()
                .unwrap(),
            PermissionPolicy::Yolo
        );
        assert_eq!(
            parse(&["-p", "-A", "hi"])
                .unwrap()
                .permission_policy()
                .unwrap(),
            PermissionPolicy::AutoReview
        );
        let both = parse(&["--yolo", "-A", "hi"]).unwrap();
        let err = both.validate().unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    /// Print, term, and recipe all resolve their policy from the same
    /// top-level flags, and print lands it on the query itself.
    #[test]
    fn every_entry_point_maps_the_same_policy() {
        for args in [
            vec!["--yolo", "-p", "hi"],
            vec!["--yolo", "term", "run", "fix it"],
            vec!["--yolo", "recipe", "run", "audit"],
        ] {
            let cli = parse(&args).unwrap();
            assert_eq!(
                cli.permission_policy().unwrap(),
                PermissionPolicy::Yolo,
                "{args:?}"
            );
        }
        let cli = parse(&["-p", "-A", "hi"]).unwrap();
        let query = HeadlessQuery::for_print(&cli, "hi".into(), None).unwrap();
        assert_eq!(query.policy, PermissionPolicy::AutoReview);
        assert_eq!(query.session_id, None);
        // The impossible pair is rejected at the mapping, not just validate.
        let both = parse(&["-p", "--yolo", "-A", "hi"]).unwrap();
        assert!(HeadlessQuery::for_print(&both, "hi".into(), None).is_err());
    }

    #[test]
    fn session_flag_accepts_resume_alias() {
        let id = "01965087-4c71-7f00-8000-000000000000";
        let a = parse(&["-s", id]).unwrap();
        let b = parse(&["--resume", id]).unwrap();
        assert_eq!(a.resume_session().unwrap(), b.resume_session().unwrap());
        assert!(a.resume_session().unwrap().is_some());
    }

    #[test]
    fn mode_flow_is_rejected_until_task_99() {
        let cli = parse(&["--mode", "flow"]).unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.to_string().contains("flow"));
    }

    #[test]
    fn sdk_mode_needs_print_and_stream_json_input() {
        assert!(!parse(&["-p", "hi"]).unwrap().is_sdk_mode());
        assert!(!parse(&["-p", "hi"]).unwrap().is_sdk_mode());
        let sdk = parse(&["--input-format", "stream-json", "-p", "hi"]).unwrap();
        assert!(sdk.is_sdk_mode());
    }

    #[test]
    fn stream_json_output_is_accepted_in_print_mode() {
        let cli = parse(&["-p", "--output-format", "stream-json", "hi"]).unwrap();
        assert!(cli.validate().is_ok());
        let cli = parse(&["-p", "--input-format", "stream-json", "hi"]).unwrap();
        assert!(
            cli.validate()
                .unwrap_err()
                .to_string()
                .contains("stream-json")
        );
    }

    #[test]
    fn print_only_flags_require_print() {
        for flag in ["--verbose", "--exit-on-done", "--include-partial-messages"] {
            let cli = parse(&[flag]).unwrap();
            assert!(cli.validate().unwrap_err().to_string().contains("--print"));
        }
    }

    #[test]
    fn continue_and_session_are_mutually_exclusive() {
        let cli = parse(&["-c", "-s", "01965087-4c71-7f00-8000-000000000000"]).unwrap();
        assert!(
            cli.validate()
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
    }

    // -- H.9 session resolution: real temp state dirs, no network --

    fn plan_for(args: &[&str], tmp: &tempfile::TempDir, cwd: &str) -> Result<ResumePlan> {
        let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
        parse(args).unwrap().resolve_session_plan(&dir, cwd)
    }

    fn seed_session(tmp: &tempfile::TempDir, cwd: &str, model: &str, turns: &[&str]) -> SessionRef {
        use crate::headless::StoredSession;
        let mut session = StoredSession::new(model, cwd);
        session.title = "seeded".into();
        session.usage_by_model_mut().insert(
            model.to_string(),
            crate::storage::sessions::StoredTokenUsage {
                input: 11,
                output: 7,
                ..Default::default()
            },
        );
        session.replace_messages(
            turns
                .iter()
                .map(|turn| crate::history::Message::user(*turn))
                .collect(),
        );
        let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
        session.save(&dir).unwrap();
        session.id.clone()
    }

    fn session_path(tmp: &tempfile::TempDir, id: &SessionRef) -> std::path::PathBuf {
        tmp.path()
            .join("sessions")
            .join(format!("{}.jsonl", id.id()))
    }

    /// `--continue` picks the newest session for the cwd; sessions from
    /// other cwds never bleed in (H.9 acceptance).
    #[test]
    fn continue_resolves_the_newest_session_for_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/project";
        let older = seed_session(&tmp, cwd, "anthropic/a", &["older"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let newer = seed_session(&tmp, cwd, "anthropic/b", &["newer"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        seed_session(&tmp, "/elsewhere", "anthropic/c", &["other cwd"]);
        let plan = plan_for(&["-p", "-c", "hi"], &tmp, cwd).unwrap();
        match plan {
            ResumePlan::Resume { id, model } => {
                assert_eq!(id, newer);
                assert_ne!(id, older);
                assert_eq!(model.as_deref(), Some("anthropic/b"));
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn continue_without_sessions_starts_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            plan_for(&["-p", "-c", "hi"], &tmp, "/project").unwrap(),
            ResumePlan::Fresh { id: None }
        );
    }

    #[test]
    fn malformed_and_missing_session_ids_fail_fast() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(plan_for(&["-p", "-s", "not-a-craft-id", "hi"], &tmp, "/p").is_err());
        // A well-formed id with no record behind it is a hard error too:
        // silent degradation would discard the explicit resume intent.
        let id = "01965087-4c71-7f00-8000-000000000000";
        let err = plan_for(&["-p", "-s", id, "hi"], &tmp, "/p").unwrap_err();
        assert!(err.to_string().contains("could not be loaded"));
    }

    /// A corrupt source record fails the run up front — both for an explicit
    /// `--session` and for the `--continue` latest, even when an older
    /// sibling session could have been silently downgraded to (H.9).
    #[test]
    fn corrupt_records_fail_explicit_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/project";
        let id = seed_session(&tmp, cwd, "anthropic/a", &["turn one"]);
        std::fs::write(session_path(&tmp, &id), b"{\"t\":\"header\"}\n").unwrap();
        assert!(
            plan_for(&["-p", "-s", &id.to_string(), "hi"], &tmp, cwd)
                .unwrap_err()
                .to_string()
                .contains("could not be loaded")
        );
        assert!(plan_for(&["-p", "-c", "hi"], &tmp, cwd).is_err());
    }

    /// A corrupt NEWEST session must not silently downgrade `--continue` to
    /// an older sibling for the same cwd (the index names the newest).
    #[test]
    fn continue_hard_errors_on_a_corrupt_newest_with_older_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/project";
        seed_session(&tmp, cwd, "anthropic/a", &["older"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let newest = seed_session(&tmp, cwd, "anthropic/b", &["newest"]);
        std::fs::write(session_path(&tmp, &newest), b"garbage\n").unwrap();
        let err = plan_for(&["-p", "-c", "hi"], &tmp, cwd).unwrap_err();
        assert!(err.to_string().contains("could not be loaded"));
    }

    /// A stale cwd-index entry for a deleted session is not corruption:
    /// `--continue` falls back to the scan, and with nothing left it starts
    /// fresh instead of erroring forever (H.9).
    #[test]
    fn continue_survives_a_stale_index_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/project";
        let id = seed_session(&tmp, cwd, "anthropic/a", &["gone"]);
        std::fs::remove_file(session_path(&tmp, &id)).unwrap();
        assert_eq!(
            plan_for(&["-p", "-c", "hi"], &tmp, cwd).unwrap(),
            ResumePlan::Fresh { id: None }
        );
    }

    /// Fork copies the source under a new id (or `--session-id`) without
    /// touching the source file (H.9 acceptance).
    #[test]
    fn fork_copies_the_source_without_modifying_it() {
        use crate::headless::StoredSession;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = "/project";
        let source_id = seed_session(&tmp, cwd, "anthropic/a", &["first", "second"]);
        let before = std::fs::read(session_path(&tmp, &source_id)).unwrap();

        let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
        let cli = parse(&["-p", "-s", &source_id.to_string(), "--fork-session", "hi"]).unwrap();
        let new_id = match cli.resolve_session_plan(&dir, cwd).unwrap() {
            ResumePlan::Fork { new_id, model } => {
                assert_eq!(model.as_deref(), Some("anthropic/a"));
                new_id
            }
            other => panic!("expected Fork, got {other:?}"),
        };
        assert_ne!(new_id, source_id);
        // The source is byte-identical and the copy carries everything over.
        assert_eq!(
            std::fs::read(session_path(&tmp, &source_id)).unwrap(),
            before
        );
        let forked = StoredSession::load(new_id.id(), &dir).unwrap();
        assert_eq!(forked.messages().len(), 2);
        assert_eq!(forked.title, "seeded");
        assert_eq!(forked.model, "anthropic/a");
        assert_eq!(
            forked
                .usage_by_model()
                .get("anthropic/a")
                .map(|u| u.total()),
            Some(18)
        );

        // fork + --session-id persists the copy under the supplied id.
        let named = "01965087-4c71-7f00-8000-00000000000f";
        let cli = parse(&[
            "-p",
            "-s",
            &source_id.to_string(),
            "--fork-session",
            "--session-id",
            named,
            "hi",
        ])
        .unwrap();
        match cli.resolve_session_plan(&dir, cwd).unwrap() {
            ResumePlan::Fork { new_id, .. } => assert_eq!(new_id, named.parse().unwrap()),
            other => panic!("expected Fork, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(session_path(&tmp, &source_id)).unwrap(),
            before
        );
    }

    /// Stored model restores when `--model` is absent; the explicit flag
    /// wins; `--session-id` rides through as the new persistence id (H.9).
    #[test]
    fn apply_resume_plan_restores_the_stored_model() {
        let id: SessionRef = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let stored = ResumePlan::Resume {
            id: id.clone(),
            model: Some("anthropic/stored".into()),
        };

        let cli = parse(&["-p", "-c", "hi"]).unwrap();
        let mut query = HeadlessQuery::for_print(&cli, "hi".into(), None).unwrap();
        assert_eq!(query.model, None);
        apply_resume_plan(&cli, &mut query, stored);
        assert_eq!(query.session_id.as_ref(), Some(&id));
        assert_eq!(query.model.as_deref(), Some("anthropic/stored"));

        // Explicit --model overrides the stored spec.
        let cli = parse(&["-p", "-c", "-m", "openai/explicit", "hi"]).unwrap();
        let mut query = HeadlessQuery::for_print(&cli, "hi".into(), None).unwrap();
        apply_resume_plan(
            &cli,
            &mut query,
            ResumePlan::Fork {
                new_id: id.clone(),
                model: Some("anthropic/stored".into()),
            },
        );
        assert_eq!(query.model.as_deref(), Some("openai/explicit"));

        // Fresh keeps passing --session-id through as the persistence id.
        let named = "01965087-4c71-7f00-8000-00000000000f";
        let cli = parse(&["-p", "--session-id", named, "hi"]).unwrap();
        let mut query = HeadlessQuery::for_print(&cli, "hi".into(), None).unwrap();
        apply_resume_plan(&cli, &mut query, ResumePlan::Fresh { id: None });
        assert_eq!(query.session_id, None);
        assert_eq!(
            plan_for(
                &["-p", "--session-id", named, "hi"],
                &tempfile::tempdir().unwrap(),
                "/p"
            )
            .unwrap(),
            ResumePlan::Fresh {
                id: Some(named.parse().unwrap())
            }
        );
    }

    #[test]
    fn fork_session_needs_a_session_to_load() {
        assert!(parse(&["--fork-session"]).unwrap().validate().is_err());
        let cli = parse(&[
            "-p",
            "-s",
            "01965087-4c71-7f00-8000-000000000000",
            "--fork-session",
            "hi",
        ])
        .unwrap();
        assert!(cli.validate().is_ok());
    }

    /// H.9: `--session-id` names a NEW session; without `--fork-session` it
    /// cannot be combined with the resume flags.
    #[test]
    fn session_id_needs_fork_to_combine_with_resume_flags() {
        let id = "01965087-4c71-7f00-8000-000000000000";
        for args in [
            vec!["-p", "--session-id", id, "-s", id, "hi"],
            vec!["-p", "--session-id", id, "-c", "hi"],
        ] {
            let cli = parse(&args).unwrap();
            assert!(
                cli.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("--session-id"),
                "{args:?}"
            );
        }
        for args in [
            vec!["-p", "--session-id", id, "-s", id, "--fork-session", "hi"],
            vec!["-p", "--session-id", id, "-c", "--fork-session", "hi"],
        ] {
            assert!(parse(&args).unwrap().validate().is_ok(), "{args:?}");
        }
    }

    #[test]
    fn bad_session_ids_are_rejected() {
        let cli = parse(&["-s", "not-a-craft-id"]).unwrap();
        assert!(cli.resume_session().is_err());
        assert!(parse_session_ref("not-a-craft-id").is_err());
    }

    #[test]
    fn effective_preamble_replaces_and_appends() {
        let base = "config text";
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.effective_preamble(base), "config text");
        let cli = parse(&["--system-prompt", "override"]).unwrap();
        assert_eq!(cli.effective_preamble(base), "override");
        let cli = parse(&["--append-system-prompt", "extra"]).unwrap();
        assert_eq!(cli.effective_preamble(base), "config text\n\nextra");
        let cli = parse(&["--system-prompt", "a", "--append-system-prompt", "b"]).unwrap();
        assert_eq!(cli.effective_preamble(base), "a\n\nb");
    }

    #[test]
    fn hidden_compat_flags_parse_and_warn() {
        let cli = parse(&["--fallback-model", "x", "--strict-mcp-config", "hi"]).unwrap();
        assert!(cli.validate().is_ok());
        assert_eq!(cli.fallback_model.as_deref(), Some("x"));
    }

    #[test]
    fn subcommands_still_parse() {
        let cli = parse(&["completions", "zsh"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Completions { .. })));
        let cli = parse(&["acp"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Acp)));
    }

    #[test]
    fn g2_subcommands_parse_with_the_reference_flags() {
        assert!(matches!(
            cli(parse(&["models"]).unwrap()).command,
            Some(Commands::Models)
        ));
        let cli = parse(&["stats", "--sessions"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Stats { sessions: true })
        ));
        let cli = parse(&["stats"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Stats { sessions: false })
        ));
        let cli = parse(&["doctor", "--export"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Doctor { export: true })
        ));
    }

    fn cli(cli: Cli) -> Cli {
        cli
    }

    #[test]
    fn prompt_subcommand_parses_variants_and_flags() {
        let cli = parse(&["prompt"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Prompt {
                variant: PromptVariant::System,
                plan: false,
                tools: false,
                names: false,
            })
        ));
        let cli = parse(&["prompt", "research"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Prompt {
                variant: PromptVariant::Research,
                ..
            })
        ));
        let cli = parse(&["prompt", "general", "--plan"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Prompt {
                variant: PromptVariant::General,
                plan: true,
                ..
            })
        ));
        let cli = parse(&["prompt", "--tools", "--names"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Prompt {
                tools: true,
                names: true,
                ..
            })
        ));
    }

    #[test]
    fn g7_update_and_rollback_subcommands_parse() {
        let cli = parse(&["update", "-y", "--no-color"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Update {
                yes: true,
                no_color: true
            })
        ));
        let cli = parse(&["update"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Update {
                yes: false,
                no_color: false
            })
        ));
        assert!(matches!(
            parse(&["rollback"]).unwrap().command,
            Some(Commands::Rollback)
        ));
    }

    #[test]
    fn prompt_names_flag_requires_tools() {
        assert!(parse(&["prompt", "--names"]).is_err());
        assert!(parse(&["prompt", "--tools", "--names"]).is_ok());
    }

    #[test]
    fn tool_specs_normalize_pascal_snake_and_mcp_forms() {
        use crate::permissions::ToolKey;
        assert_eq!(
            Cli::normalize_tool_spec("Read").unwrap(),
            ToolKey::native("read")
        );
        assert_eq!(
            Cli::normalize_tool_spec("ViewImage").unwrap(),
            ToolKey::native("view_image")
        );
        assert_eq!(
            Cli::normalize_tool_spec("bash").unwrap(),
            ToolKey::native("bash")
        );
        // Concatenated registry names: the snake pass, then the joined form.
        assert_eq!(
            Cli::normalize_tool_spec("WebFetch").unwrap(),
            ToolKey::native("webfetch")
        );
        assert_eq!(
            Cli::normalize_tool_spec("web_fetch").unwrap(),
            ToolKey::native("webfetch")
        );
        assert_eq!(
            Cli::normalize_tool_spec("MultiEdit").unwrap(),
            ToolKey::native("multiedit")
        );
        assert_eq!(
            Cli::normalize_tool_spec("github__create_issue").unwrap(),
            ToolKey::McpTool {
                server: "github".into(),
                tool: "create_issue".into()
            }
        );
        assert_eq!(
            Cli::normalize_tool_spec("mcp__github__create_issue").unwrap(),
            ToolKey::McpTool {
                server: "github".into(),
                tool: "create_issue".into()
            }
        );
        assert_eq!(
            Cli::normalize_tool_spec("mcp__github").unwrap(),
            ToolKey::McpServer {
                server: "github".into()
            }
        );
    }

    #[test]
    fn bad_tool_specs_error_with_guidance() {
        for bad in ["*", "NotATool", "", "mcp__", "my_server__tool"] {
            assert!(Cli::normalize_tool_spec(bad).is_err(), "{bad:?}");
        }
        let wildcard = Cli::normalize_tool_spec("*").unwrap_err().to_string();
        assert!(wildcard.contains("permissions.bml"), "{wildcard}");
        let unknown = Cli::normalize_tool_spec("NotATool")
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown tool"), "{unknown}");
        assert!(unknown.contains("Valid tools"), "{unknown}");
        assert!(unknown.contains("read"), "names the valid tools: {unknown}");
    }

    #[test]
    fn tool_policy_produces_scope_universal_rules() {
        use crate::permissions::Effect;
        let cli = parse(&[
            "--allowed-tools",
            "Read,read,Bash",
            "--disallowed-tools",
            "mcp__github",
        ])
        .unwrap();
        let rules = cli.tool_policy().unwrap();
        // `Read` and `read` collapse to one rule; effects are as flagged.
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().all(|r| r.scope.is_none()));
        let bash = rules
            .iter()
            .find(|r| r.tool == crate::permissions::ToolKey::native("bash"))
            .expect("bash allow rule");
        assert_eq!(bash.effect, Effect::Allow);
        let github = rules
            .iter()
            .find(|r| matches!(r.tool, crate::permissions::ToolKey::McpServer { .. }))
            .expect("github deny rule");
        assert_eq!(github.effect, Effect::Deny);
        assert!(
            parse(&["-p", "hi"])
                .unwrap()
                .tool_policy()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn overlapping_allow_and_deny_lists_error_naming_the_tool() {
        let cli = parse(&[
            "--allowedTools",
            "Read,bash",
            "--disallowed-tools",
            "Bash",
            "-p",
            "hi",
        ])
        .unwrap();
        let err = cli.validate().unwrap_err().to_string();
        assert!(err.contains("bash"), "{err}");
    }

    #[test]
    fn permission_mode_maps_postures_and_rejects_unsupported() {
        assert_eq!(
            parse(&["--permission-mode", "default", "-p", "hi"])
                .unwrap()
                .permission_policy()
                .unwrap(),
            PermissionPolicy::Standard
        );
        assert_eq!(
            parse(&["--permission-mode", "bypassPermissions", "-p", "hi"])
                .unwrap()
                .permission_policy()
                .unwrap(),
            PermissionPolicy::Yolo
        );
        let err = parse(&["--permission-mode", "acceptEdits", "hi"])
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("acceptEdits"), "{err}");
        let err = parse(&["--permission-mode", "bypassPermissions", "-A", "hi"])
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("auto-review"), "{err}");
    }

    #[test]
    fn permission_mode_plan_maps_to_plan_mode() {
        let cli = parse(&["--permission-mode", "plan", "-p", "hi"]).unwrap();
        assert!(cli.validate().is_ok());
        assert_eq!(cli.run_mode(), CliMode::Plan);
        assert_eq!(
            parse(&["--mode", "plan", "--permission-mode", "plan", "-p", "hi"])
                .unwrap()
                .run_mode(),
            CliMode::Plan
        );
        let err = parse(&["--mode", "build", "--permission-mode", "plan", "-p", "hi"])
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("--mode build"), "{err}");
        // Without the flag the mode still defaults to build.
        assert_eq!(parse(&["-p", "hi"]).unwrap().run_mode(), CliMode::Build);
    }

    #[test]
    fn print_maps_the_cli_tool_policy_onto_the_query() {
        let cli = parse(&["-p", "--disallowed-tools", "bash", "hi"]).unwrap();
        let query = HeadlessQuery::for_print(&cli, "hi".into(), None).unwrap();
        assert_eq!(query.tool_policy.len(), 1);
        assert_eq!(
            query.tool_policy[0].tool,
            crate::permissions::ToolKey::native("bash")
        );
    }
}

/// Resolve the initial prompt for a run: the positional `PROMPT` wins; else
/// piped stdin (when `stdin_attached` is false) is read to a string; a
/// terminal stdin yields `None`. The result is trimmed; whitespace-only
/// input collapses to `None` so the TUI starts normally. Shared by
/// `--print` (which still errors on `None`) and the interactive TUI.
pub fn resolve_prompt_input(
    cli_prompt: Option<String>,
    stdin: &mut impl std::io::Read,
    stdin_attached: bool,
) -> Result<Option<String>> {
    let raw = match cli_prompt {
        Some(prompt) => prompt,
        None if !stdin_attached => {
            let mut buf = String::new();
            stdin.read_to_string(&mut buf).map_err(|e| {
                InvalidSnafu {
                    reason: format!("reading the prompt from stdin: {e}"),
                }
                .build()
            })?;
            buf
        }
        None => return Ok(None),
    };
    let trimmed = raw.trim().to_string();
    Ok((!trimmed.is_empty()).then_some(trimmed))
}

/// `craft --print` (G.3): run one prompt to completion against the
/// configured providers and emit text, JSONL (`--output-format
/// stream-json`), or a verbose transcript (`--verbose`). SDK-mode
/// stream-json input remains unported.
pub async fn run_print(cli: &Cli, mut config: crate::config::Config) -> Result<()> {
    cli.apply_thinking(&mut config)?;
    // One warning per unimplemented option, in a stable order.
    for flag in ["--image", "--verbose", "--include-partial-messages"] {
        let set = match flag {
            "--image" => !cli.images.is_empty(),
            "--verbose" => cli.verbose,
            _ => cli.include_partial_messages,
        };
        if set {
            eprintln!("warning: {flag} is accepted but not implemented yet (task 84)");
        }
    }

    let prompt = match resolve_prompt_input(
        cli.initial_prompt.clone(),
        &mut std::io::stdin(),
        std::io::stdin().is_terminal(),
    )? {
        Some(prompt) => prompt,
        None => {
            return InvalidSnafu {
                reason: "--print needs a prompt argument or a piped stdin prompt".to_string(),
            }
            .fail();
        }
    };

    // Session resolution (H.9): --continue / --session / --fork-session get
    // their print-mode semantics here, before any model work.
    let cwd = std::env::current_dir().map_err(|e| {
        InvalidSnafu {
            reason: format!("resolving the current directory: {e}"),
        }
        .build()
    })?;
    let plan = match StateDir::resolve() {
        Ok(dir) => cli.resolve_session_plan(&dir, &cwd.display().to_string())?,
        Err(e) => {
            if cli.fork_session || cli.continue_session || cli.session.is_some() {
                return InvalidSnafu {
                    reason: format!("session storage is unavailable, cannot resume or fork: {e}"),
                }
                .fail();
            }
            ResumePlan::Fresh { id: None }
        }
    };
    let mut query = HeadlessQuery::for_print(cli, prompt, None)?;
    apply_resume_plan(cli, &mut query, plan);

    config.agent.preamble = cli.effective_preamble(&config.agent.preamble);
    run_headless_query(config, query).await
}

/// Wire a resolved [`ResumePlan`] into the query: the session id the run
/// persists under, and the stored `provider/model` when `--model` is absent
/// (an explicit flag always wins, H.9).
fn apply_resume_plan(cli: &Cli, query: &mut HeadlessQuery, plan: ResumePlan) {
    match plan {
        ResumePlan::Fresh { id } => query.session_id = id,
        ResumePlan::Resume { id, model } | ResumePlan::Fork { new_id: id, model } => {
            query.session_id = Some(id);
            if cli.model.is_none() {
                query.model = model;
            }
        }
    }
}

/// Extra environment context (e.g. shell history) injected before the prompt.
fn inject_context(prompt: &str, context: &[String]) -> String {
    if context.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from("<context>\n");
    for block in context {
        out.push_str(block);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("</context>\n\n");
    out.push_str(prompt);
    out
}

/// PascalCase → snake_case (`Read`→`read`, `ViewImage`→`view_image`);
/// already-snake names pass through unchanged.
fn pascal_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// One headless agent query in print mode. Both `--print` and
/// `craft term run` funnel through here; `config.agent.preamble` must
/// already be the effective preamble.
pub struct HeadlessQuery {
    pub prompt: String,
    /// Context blocks wrapped in `<context>` before the prompt.
    pub context: Vec<String>,
    pub images: Vec<std::path::PathBuf>,
    pub model: Option<String>,
    pub output_format: OutputFormat,
    pub verbose: bool,
    pub mode: CliMode,
    pub session_id: Option<SessionRef>,
    /// Whether `--yolo` / `-A` bypass the permission gate.
    pub policy: PermissionPolicy,
    /// CLI tool policy (`--allowed-tools` / `--disallowed-tools`) as
    /// permission rules, applied to the run's permission engine before
    /// the first turn. Empty for surfaces that do not expose the flags
    /// (`term run`, `recipe run`).
    pub tool_policy: Vec<PermissionRule>,
}

impl HeadlessQuery {
    /// The `--print` mapping, straight off the parsed CLI, so the
    /// flag→policy mapping is unit-testable without loading config or
    /// touching the network.
    pub fn for_print(cli: &Cli, prompt: String, session_id: Option<SessionRef>) -> Result<Self> {
        Ok(Self {
            prompt,
            context: Vec::new(),
            images: cli.images.clone(),
            model: cli.model.clone(),
            output_format: cli.output_format.clone(),
            verbose: cli.verbose,
            mode: cli.run_mode(),
            session_id,
            policy: cli.permission_policy()?,
            tool_policy: cli.tool_policy()?,
        })
    }
}

pub async fn run_headless_query(config: crate::config::Config, q: HeadlessQuery) -> Result<()> {
    let _ = tokio::time::timeout(crate::models_dev::FETCH_BUDGET, crate::models_dev::warm()).await;
    let prompt = inject_context(&q.prompt, &q.context);
    // Fails fast: silently dropping an image the caller explicitly attached
    // would be worse than erroring.
    let images = crate::print::load_images(&q.images)?;

    // Model resolution: `-m provider/model-id`, else the first configured
    // completion provider's first catalog entry.
    let (provider_name, provider_config) =
        match &q.model {
            Some(spec) => {
                let (provider, _model) = spec.split_once('/').ok_or_else(|| {
                    InvalidSnafu {
                        reason: format!("--model expects provider/model-id, got {spec:?}"),
                    }
                    .build()
                })?;
                let config =
                    config.providers.get(provider).ok_or_else(|| {
                        InvalidSnafu {
                    reason: format!(
                        "unknown provider {provider:?} in --model {spec:?} (configured: {})",
                        config.providers.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                }
                .build()
                    })?;
                (provider.to_string(), config.clone())
            }
            None => config
                .providers
                .iter()
                .find(|(_, c)| c.kind != crate::providers::ProviderKind::Voyageai)
                .map(|(name, c)| (name.to_string(), c.clone()))
                .ok_or_else(|| {
                    InvalidSnafu {
                        reason: crate::setup::setup_hint(),
                    }
                    .build()
                })?,
        };
    let provider = crate::providers::Provider::from_config(&provider_config)?;
    // Discovery feeds selection (implicit path) and metadata lookup; the
    // explicit `-m` id is never catalog-checked, so a discovery miss only
    // drops the metadata, never the run.
    let catalog = provider.models(&provider_config).await;
    let model_id = match &q.model {
        Some(spec) => spec.split_once('/').expect("validated above").1.to_string(),
        None => {
            let models = catalog.as_ref().map_err(|e| {
                InvalidSnafu {
                    reason: format!("discovering models for {provider_name:?}: {e}"),
                }
                .build()
            })?;
            models
                .first()
                .ok_or_else(|| {
                    InvalidSnafu {
                        reason: format!("provider {provider_name:?} has no models"),
                    }
                    .build()
                })?
                .id
                .clone()
        }
    };
    let model = provider.configured_model(&provider_config, &model_id)?;
    let (context_length, max_output_tokens) = catalog
        .as_ref()
        .map(|models| {
            crate::runtime::catalog_metadata(
                &std::collections::BTreeMap::from([(provider_name.clone(), models.clone())]),
                &provider_name,
                &model_id,
            )
        })
        .unwrap_or((None, None));

    let cwd = std::env::current_dir().map_err(|e| {
        crate::error::InvalidSnafu {
            reason: format!("resolving the current directory: {e}"),
        }
        .build()
    })?;
    let cwd_str = cwd.display().to_string();
    // Headless session environment: instructions + permissions + workspace
    // + connected MCP, through the shared runtime setup contract. The
    // permission policy lands on the engine before the gate reads it, and
    // the gate below enforces it on every dispatch (batch children and
    // subagents included).
    let env = crate::runtime::workspace_env(
        &cwd,
        crate::runtime::McpStartup::Connected,
        false,
        crate::sandbox::SandboxPolicy::resolve(&config, q.policy == PermissionPolicy::Yolo),
    )
    .await?;
    if let Some(note) = &env.sandbox_note {
        eprintln!("{note}");
    }
    match q.policy {
        PermissionPolicy::Standard => {}
        PermissionPolicy::Yolo => env.permissions.set_yolo(true),
        PermissionPolicy::AutoReview => env.permissions.set_auto_review(true),
    }
    env.permissions.add_cli_rules(q.tool_policy);
    // Destructive-hinted MCP tools must force a decision even under an
    // allow rule, exactly like the TUI/ACP gates: feed the engine the
    // published annotations before the gate reads it.
    if let Some(mcp) = env.workspace.mcp() {
        env.permissions.sync_mcp_annotations(&mcp);
    }
    let reviewer = (q.policy == PermissionPolicy::AutoReview)
        .then(|| crate::auto_review::reviewer_for(model.clone()));
    let before = Some(Arc::new(crate::headless::HeadlessGate::new(
        Arc::clone(&env.permissions),
        reviewer,
    )) as Arc<dyn BeforeExecute>);
    let workspace = env.workspace;
    let state_dir = crate::storage::StateDir::resolve().ok();

    let mode = match q.mode {
        CliMode::Plan => crate::run::AgentMode::Plan(
            state_dir
                .as_ref()
                .and_then(|dir| crate::storage::plans::new_plan_path(dir).ok())
                .unwrap_or_else(|| std::path::PathBuf::from("plans/plan.md")),
        ),
        _ => crate::run::AgentMode::Build,
    };

    // Headless compaction (the print-mode fix): the session's shared state
    // and the selected model's window make output caps window-clamped and
    // give the run loop in-run overflow recovery. Without a window the
    // stages simply never cross a threshold.
    let compaction_state = crate::runtime::new_compaction_state(
        crate::run::shared_cache(),
        crate::run::shared_guardrails(),
    );
    let compaction_ctx = crate::runtime::compaction_ctx(compaction_state, &config, context_length);
    let resolved = crate::runtime::ResolvedModel {
        provider: provider_name.clone(),
        model_id: model_id.clone(),
        context_length,
        max_output_tokens,
    };
    let params = crate::runtime::run_policy(crate::runtime::RunPolicyInputs {
        config: &config,
        cwd: &cwd_str,
        instructions_text: &env.instructions.text,
        mode: &mode,
        model: &resolved,
        compaction: Some(compaction_ctx),
        recency: None,
        retry: crate::run::RetryCtx::default(),
        fast: false,
        thinking: None,
        max_turns: crate::runtime::MaxTurns::FromConfig,
    });

    let model_label = params
        .model_spec
        .as_ref()
        .map(|spec| spec.to_string())
        .unwrap_or_else(|| model_id.clone());
    let handle = crate::headless::spawn(crate::headless::HeadlessParams {
        model,
        model_spec: params.model_spec.clone(),
        run: params,
        workspace,
        before,
        prompt,
        images,
        initial_wd: cwd,
        state_dir,
        session_id: q.session_id,
    });

    // G.3 print mode: raw text or stream-json JSONL output. Unlike the
    // reference (whose print mode exits 0 on agent errors), a failed run is
    // a failure exit.
    match crate::print::emit(handle, &q.output_format, q.verbose, &model_label).await? {
        Some(message) => Err(crate::error::InvalidSnafu { reason: message }.build()),
        None => Ok(()),
    }
}
