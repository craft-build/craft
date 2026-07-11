use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::Result;
use color_eyre::eyre::bail;

use craft_agent::tools::{all_builtin_tool_names, is_builtin_tool};

use crate::print::OutputFormat;

#[derive(Clone, ValueEnum, Default)]
pub enum PromptVariant {
    #[default]
    System,
    Research,
    General,
}

#[derive(Clone, ValueEnum, Default)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Clone, ValueEnum, Default)]
pub enum CompletionShell {
    #[default]
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

#[derive(Clone, ValueEnum, Default)]
pub enum CliMode {
    #[default]
    Build,
    Plan,
    Flow,
}

#[derive(Parser)]
#[command(name = "craft", version, about = "AI coding agent for the terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Non-interactive mode. Runs the prompt and exits. Compatible with Claude Code's --print flag
    #[arg(short, long)]
    pub print: bool,

    /// Attach an image to the prompt in --print mode as vision content (repeatable)
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<std::path::PathBuf>,

    /// Model spec (provider/model-id). Defaults to last used model, or claude-opus-4-6
    #[arg(short, long)]
    pub model: Option<String>,

    /// Include full turn-by-turn messages in --print output
    #[arg(long)]
    pub verbose: bool,

    /// Resume the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(short = 's', long, alias = "resume")]
    pub session: Option<String>,

    /// Output format for --print mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Initial mode (build, plan, flow). Selects the agent mode on startup.
    #[arg(long, value_enum, default_value_t = CliMode::Build)]
    pub mode: CliMode,

    /// Input format (text or stream-json for SDK mode)
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Skip loading custom commands from .craft/commands, .claude/commands, etc.
    #[arg(long)]
    pub no_commands: bool,

    /// Disable rtk command rewriting
    #[arg(long)]
    pub no_rtk: bool,

    /// Disable the Lua plugin system
    #[arg(long)]
    pub no_plugins: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long, alias = "dangerously-skip-permissions")]
    pub yolo: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools (comma-separated).
    #[arg(long, value_delimiter = ',', visible_alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Session ID for SDK mode
    #[arg(long)]
    pub session_id: Option<String>,

    /// Fork the loaded session under a new ID
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

    /// Permission mode for SDK
    #[arg(long)]
    pub permission_mode: Option<String>,

    /// Include partial streaming messages in SDK output
    #[arg(long)]
    pub include_partial_messages: bool,

    /// Permission prompt tool (accepted for compat, used in SDK mode)
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
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models,
    /// Generate shell completions for the given shell (print to stdout)
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Show cost and usage stats from the persistent ledger
    Stats {
        /// Show per-session breakdown instead of the default per-model view
        #[arg(long)]
        sessions: bool,
    },
    /// Run the outline tool on a file to see how it looks like
    Outline { path: String },
    /// Manage MCP server authentication
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Update craft to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Disable syntax highlighting
        #[arg(long)]
        no_color: bool,
    },
    /// Rollback to the previous version
    Rollback,
    /// Run craft as an Agent Client Protocol (ACP) server over stdio
    Acp {
        /// Skip all permission prompts (allow everything)
        #[arg(long)]
        yolo: bool,
    },
    /// Run a headless agent query (a prompt or a recipe file) and print the result
    Run(RunCommand),
    /// Run the Flow multi-stage pipeline on a request, or prune old workstreams
    Flow(FlowCommand),
    /// Manage the in-project local wiki knowledge base (`.wiki/`)
    Wiki(WikiCommand),
    /// Run deterministic review checks against the current diff
    Review(ReviewCommand),
    /// Terminal shell integration: transparent command logging and `@craft` alias
    Term {
        #[command(subcommand)]
        action: TermAction,
    },
    /// Diagnose and self-heal provider configuration
    Doctor {
        /// Export a JSON diagnostics report instead of running self-heal
        #[arg(long)]
        export: bool,
    },
    /// Show the rendered system prompt or tool definitions
    Prompt {
        /// Prompt variant: system (default), research, general
        #[arg(value_enum, default_value_t = PromptVariant::System)]
        variant: PromptVariant,
        /// Append the plan mode reminder to the system prompt
        #[arg(long)]
        plan: bool,
        /// Show tool definitions (JSON) instead of prompt text
        #[arg(long)]
        tools: bool,
        /// With --tools: show only tool names, one per line
        #[arg(long, requires = "tools")]
        names: bool,
    },
    /// Data migration utilities
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(Parser)]
pub struct RunCommand {
    /// A prompt to run, or a path to a recipe file (.yaml/.yml/.json)
    pub prompt: Option<String>,
    /// Model spec (provider/model-id)
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// Output format for the result
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,
    /// Don't persist a session log for this run
    #[arg(long)]
    pub no_session: bool,
    /// Suppress non-essential diagnostic output
    #[arg(long)]
    pub quiet: bool,
    /// Skip all permission prompts (allow everything)
    #[arg(long)]
    pub yolo: bool,
    /// Disable the Lua plugin system
    #[arg(long)]
    pub no_plugins: bool,
    /// Recipe parameter overrides (key=value), repeatable
    #[arg(long, value_name = "KEY=VALUE")]
    pub param: Vec<String>,
    /// Maximum number of agent turns
    #[arg(long)]
    pub max_turns: Option<u32>,
    /// Pre-approve tools (comma-separated), accepts PascalCase or snake_case
    #[arg(long, value_delimiter = ',')]
    pub allowed_tools: Vec<String>,
}

#[derive(Parser)]
pub struct ReviewCommand {
    /// List discovered checks without executing them
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the file-sharded main pass over the current diff
    #[arg(long)]
    pub no_file_pass: bool,
    /// Exit non-zero if any findings are produced (for CI)
    #[arg(long)]
    pub fail_on_findings: bool,
    /// Only run checks whose name matches this regex
    #[arg(long)]
    pub check_filter: Option<String>,
    /// Minimum severity to include (low, medium, high, critical)
    #[arg(long)]
    pub severity: Option<String>,
    /// Model spec (provider/model-id) for checks that don't specify one
    #[arg(short = 'm', long)]
    pub model: Option<String>,
}

#[derive(Subcommand)]
pub enum FlowAction {
    /// Run the Flow pipeline on a request
    Run {
        /// The request to run Flow on (reads stdin if omitted and piped)
        request: Option<String>,
        /// Print the outcome as JSON instead of entering the TUI
        #[arg(long)]
        print: bool,
        /// Output format for --print
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output_format: OutputFormat,
        /// Resume a specific session by its ID (re-enters at the approval gate)
        #[arg(short = 's', long, alias = "resume")]
        session: Option<String>,
        /// Resume payload: "approved" to approve the goal doc, else a revised goal
        #[arg(long, short = 'p', visible_alias = "payload")]
        payload: Option<String>,
        /// Retry a previously-failed run for the session, re-entering at the
        /// failed stage using persisted workstream state (implies -s <id>)
        #[arg(long)]
        retry: bool,
    },
    /// Prune Flow workstream directories older than the given age
    Gc {
        /// Age threshold, e.g. `30d`, `12h`, `45m` (humantime-style: d/h/m/s)
        #[arg(long, default_value = "30d")]
        older_than: String,
    },
}

#[derive(Parser)]
pub struct FlowCommand {
    #[command(subcommand)]
    pub action: Option<FlowAction>,
}

#[derive(Subcommand)]
pub enum WikiAction {
    /// Ingest a local file into the wiki as a structured source note with an LLM summary
    Ingest {
        /// Path to the local file to ingest
        source: std::path::PathBuf,
        /// Model spec (provider/model-id) for the summarization call
        #[arg(short = 'm', long)]
        model: Option<String>,
    },
    /// List all wiki pages and ingested sources with their titles
    List,
    /// Print a wiki page or source note by its slug
    Show { id: String },
}

#[derive(Parser)]
pub struct WikiCommand {
    #[command(subcommand)]
    pub action: WikiAction,
}

#[derive(Subcommand)]
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

#[derive(Clone, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

#[derive(Subcommand)]
pub enum MigrateAction {
    /// Migrate files from ~/.craft/ to XDG directories
    Xdg,
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Authenticate with an MCP server
    Auth {
        /// Server name from config
        server: String,
    },
    /// Remove stored OAuth credentials for an MCP server
    Logout {
        /// Server name from config
        server: String,
    },
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Authenticate with a provider (interactive if no provider specified)
    Login {
        /// Provider slug (e.g. openai, anthropic). Omit for interactive selection.
        provider: Option<String>,
    },
    /// Remove stored credentials for a provider
    Logout {
        /// Provider slug (e.g. openai)
        provider: String,
    },
    /// Show authentication status for all providers
    Status,
}

pub fn normalize_tool_name(name: &str) -> Result<String> {
    let mut result = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    if !is_builtin_tool(&result) {
        bail!(
            "unknown tool '{}'. Valid tools: {}",
            name,
            all_builtin_tool_names().join(", ")
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("Read", "read")]
    #[test_case("Bash", "bash")]
    #[test_case("CodeExecution", "code_execution")]
    #[test_case("code_execution", "code_execution"; "snake_passthrough")]
    fn normalize_tool_name_valid_inputs(input: &str, expected: &str) {
        assert_eq!(normalize_tool_name(input).unwrap(), expected);
    }

    #[test]
    fn normalize_tool_name_rejects_unknown() {
        let result = normalize_tool_name("NonExistentTool");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn normalize_tool_name_multi_edit_rejects_snake_variant() {
        assert!(normalize_tool_name("MultiEdit").is_err());
    }

    #[test_case("bash" ; "bash")]
    #[test_case("zsh" ; "zsh")]
    #[test_case("fish" ; "fish")]
    #[test_case("elvish" ; "elvish")]
    #[test_case("powershell" ; "powershell")]
    fn completion_shell_parses(shell: &str) {
        use clap::ValueEnum;
        let value = CompletionShell::from_str(shell, true);
        assert!(value.is_ok(), "failed to parse shell `{shell}`");
    }

    #[test_case(CompletionShell::Bash ; "bash")]
    #[test_case(CompletionShell::Zsh ; "zsh")]
    #[test_case(CompletionShell::Fish ; "fish")]
    #[test_case(CompletionShell::Elvish ; "elvish")]
    #[test_case(CompletionShell::Powershell ; "powershell")]
    fn completions_generate_non_empty_with_subcommands(shell: CompletionShell) {
        use clap::CommandFactory;
        use clap_complete::Shell;

        let clap_shell = match shell {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::Elvish => Shell::Elvish,
            CompletionShell::Powershell => Shell::PowerShell,
        };
        let mut cmd = Cli::command();
        let mut buf = Vec::<u8>::new();
        clap_complete::generate(clap_shell, &mut cmd, "craft", &mut buf);
        let output = String::from_utf8(buf).expect("completions are UTF-8");
        assert!(!output.is_empty());
        assert!(
            output.contains("completions"),
            "missing `completions` subcommand in output"
        );
        assert!(
            output.contains("stats"),
            "missing `stats` subcommand in output"
        );
    }
}
