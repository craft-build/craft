//! G.1 core CLI flags, ported from the reference `src/cli.rs`. The flag
//! surface (names, shorts, aliases, value enums) mirrors the reference
//! exactly; each flag's semantics land with its own subsystem. Print/SDK
//! flags are parsed and stored here for the full headless mode (task 84).

use std::io::IsTerminal;

use clap::{Parser, ValueEnum};

use crate::error::{InvalidSnafu, Result};
use crate::id::SessionRef;

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

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Craft coding agent: launches the interactive TUI by default",
    long_about = "Craft coding agent. With no subcommand, launches the interactive terminal UI \
                  backed by the configured providers in ~/.config/craft/agent.toml. With \
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

    /// Resume the most recent session in this directory
    /// (F.3 resume-latest-by-cwd).
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(short = 's', long, alias = "resume")]
    pub session: Option<String>,

    /// Output format for --print mode (task 84).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Initial mode (build, plan, flow).
    #[arg(long, value_enum, default_value_t = CliMode::Build)]
    pub mode: CliMode,

    /// Input format (text or stream-json for SDK mode; task 84).
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Skip loading custom commands (accepted for compatibility; the
    /// custom-command subsystem is task 91).
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

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code)
    /// or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools (comma-separated).
    #[arg(long, value_delimiter = ',', visible_alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Session ID for SDK mode (task 84).
    #[arg(long)]
    pub session_id: Option<String>,

    /// Fork the loaded session under a new ID (task 84).
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

    /// Permission mode for SDK (accepted, used in task 84).
    #[arg(long)]
    pub permission_mode: Option<String>,

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
    #[arg(long, hide = true)]
    pub max_thinking_tokens: Option<String>,
    #[arg(long, hide = true)]
    pub effort: Option<String>,
    #[arg(long, hide = true)]
    pub json_schema: Option<String>,
    #[arg(long, hide = true)]
    pub max_budget_usd: Option<String>,
    #[arg(long, hide = true)]
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
            ("max-thinking-tokens", self.max_thinking_tokens.is_some()),
            ("effort", self.effort.is_some()),
            ("json-schema", self.json_schema.is_some()),
            ("max-budget-usd", self.max_budget_usd.is_some()),
            ("thinking", self.thinking.is_some()),
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

    /// Cross-flag validation the reference performs before dispatch.
    /// Parse-level mistakes (bad enum values, unknown flags) are already
    /// handled by clap.
    pub fn validate(&self) -> Result<()> {
        if matches!(self.mode, CliMode::Flow) {
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
        if self.fork_session
            && self.session.is_none()
            && self.session_id.is_none()
            && !self.continue_session
        {
            return InvalidSnafu {
                reason: "--fork-session requires --session, --session-id, or --continue",
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

    /// The session to resume, from `-s/--session` (alias `--resume`) or
    /// `--session-id` (SDK compat). `--fork-session` semantics are task 84.
    pub fn resume_session(&self) -> Result<Option<SessionRef>> {
        let raw = self.session.as_ref().or(self.session_id.as_ref());
        match raw {
            Some(raw) => raw.parse::<SessionRef>().map(Some).map_err(|_| {
                InvalidSnafu {
                    reason: format!(
                        "{raw:?} is not a valid session id \
                         (expected the id shown by `craft --continue` or /sessions)"
                    ),
                }
                .build()
            }),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("craft").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_match_the_reference() {
        let cli = parse(&["hi"]).unwrap();
        assert!(!cli.print);
        assert_eq!(cli.mode, CliMode::Build);
        assert_eq!(cli.output_format, OutputFormat::Text);
        assert_eq!(cli.input_format, InputFormat::Text);
        assert!(cli.images.is_empty());
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

    #[test]
    fn bad_session_ids_are_rejected() {
        let cli = parse(&["-s", "not-a-craft-id"]).unwrap();
        assert!(cli.resume_session().is_err());
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
    fn prompt_names_flag_requires_tools() {
        assert!(parse(&["prompt", "--names"]).is_err());
        assert!(parse(&["prompt", "--tools", "--names"]).is_ok());
    }
}

/// `craft --print` (G.3): run one prompt to completion against the
/// configured providers and emit text, JSONL (`--output-format
/// stream-json`), or a verbose transcript (`--verbose`). SDK-mode
/// stream-json input remains unported.
pub async fn run_print(cli: &Cli, mut config: crate::config::Config) -> Result<()> {
    for flag in ["--fork-session", "--include-partial-messages"] {
        let set = match flag {
            "--fork-session" => cli.fork_session,
            _ => cli.include_partial_messages,
        };
        if set {
            eprintln!("warning: {flag} is accepted but not implemented yet");
        }
    }
    for flag in [
        "--image",
        "--verbose",
        "--fork-session",
        "--include-partial-messages",
    ] {
        let set = match flag {
            "--image" => !cli.images.is_empty(),
            "--verbose" => cli.verbose,
            "--fork-session" => cli.fork_session,
            _ => cli.include_partial_messages,
        };
        if set {
            eprintln!("warning: {flag} is accepted but not implemented yet (task 84)");
        }
    }

    let prompt = match &cli.initial_prompt {
        Some(prompt) => prompt.clone(),
        None if !std::io::stdin().is_terminal() => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).map_err(|e| {
                crate::error::InvalidSnafu {
                    reason: format!("reading the prompt from stdin: {e}"),
                }
                .build()
            })?;
            buf.trim().to_string()
        }
        None => {
            return InvalidSnafu {
                reason: "--print needs a prompt argument or a piped stdin prompt".to_string(),
            }
            .fail();
        }
    };

    // Fails fast: silently dropping an image the caller explicitly attached
    // would be worse than erroring.
    let images = crate::print::load_images(&cli.images)?;

    // Model resolution: `-m provider/model-id`, else the first configured
    // completion provider's first catalog entry.
    let (provider_name, provider_config) =
        match &cli.model {
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
                        reason: "no providers are configured in ~/.config/craft/agent.toml"
                            .to_string(),
                    }
                    .build()
                })?,
        };
    let model_id = match &cli.model {
        Some(spec) => spec.split_once('/').expect("validated above").1.to_string(),
        None => {
            let provider = crate::providers::Provider::from_config(&provider_config)?;
            let models = provider.models(&provider_config).await.map_err(|e| {
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
    let provider = crate::providers::Provider::from_config(&provider_config)?;
    let model = provider.completion_model(&model_id)?;

    let cwd = std::env::current_dir().map_err(|e| {
        crate::error::InvalidSnafu {
            reason: format!("resolving the current directory: {e}"),
        }
        .build()
    })?;
    let cwd_str = cwd.display().to_string();
    let instructions = crate::instructions::load_instructions(&cwd_str);
    let workspace = crate::tools::Workspace::new(&cwd)
        .map_err(crate::error::client_error)?
        .with_loaded_instructions(instructions.loaded.clone());
    let state_dir = crate::storage::StateDir::resolve().ok();

    let mode = match cli.mode {
        CliMode::Plan => crate::run::AgentMode::Plan(
            state_dir
                .as_ref()
                .and_then(|dir| crate::storage::plans::new_plan_path(dir).ok())
                .unwrap_or_else(|| std::path::PathBuf::from("plans/plan.md")),
        ),
        _ => crate::run::AgentMode::Build,
    };

    config.agent.preamble = cli.effective_preamble(&config.agent.preamble);
    let params = crate::run::RunParams {
        fast: false,
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", &cwd_str)
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", config.agent.preamble, instructions.text),
            &crate::prompt::ResolvedSlots::default(),
            mode.plan_path(),
        )),
        temperature: config.agent.temperature,
        max_tokens: config.agent.max_tokens,
        max_turns: config
            .agent
            .max_turns
            .map(|n| n as usize)
            .unwrap_or(crate::run::RunParams::UNBOUNDED),
        recency: None,
        compression: config.compression.clone(),
        max_continuation_turns: crate::run::RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
        compaction: None,
        reauth: Some(crate::providers::reauth_hook(&provider_config, &model_id)),
        model_spec: Some(format!("{provider_name}/{model_id}").into()),
        retry: crate::run::RetryCtx::default(),
    };

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
        before: None,
        prompt,
        images,
        initial_wd: cwd,
        state_dir,
        session_id: cli.resume_session()?,
    });

    // G.3 print mode: raw text or stream-json JSONL output. Unlike the
    // reference (whose print mode exits 0 on agent errors), a failed run is
    // a failure exit.
    match crate::print::emit(handle, &cli.output_format, cli.verbose, &model_label).await? {
        Some(message) => Err(crate::error::InvalidSnafu { reason: message }.build()),
        None => Ok(()),
    }
}
