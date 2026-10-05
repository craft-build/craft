//! G.2 subcommands (`models`, `stats`, `doctor`, `prompt`), ported from the
//! reference `src/cmd/subcmd.rs` and `src/cmd/subcmd/doctor.rs`. The
//! reference's plugin-host/model-policy plumbing is replaced by the config
//! file plus the Rig provider stack; doctor's `Model::from_tier` healing
//! loop iterates configured providers instead.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cli::PromptVariant;
use crate::config::Config;
use crate::error::{InvalidSnafu, Result};
use crate::paths;
use crate::prompt::{self, PromptId, ResolvedSlots, Vars};
use crate::providers::{Provider, ProviderKind};
use crate::storage::StateDir;
use crate::storage::model as model_store;
use crate::storage::stats::{CostLedger, CostSummary, format_usd};
use crate::usage::format_tokens;

fn storage() -> Result<StateDir> {
    StateDir::resolve().map_err(|e| {
        InvalidSnafu {
            reason: format!("resolve data directory: {e}"),
        }
        .build()
    })
}

/// `craft models`: one `provider/model-id` line per catalog entry. A failing
/// provider warns on stderr without aborting the other providers.
pub async fn models(config: Config) -> Result<()> {
    for (name, provider_config) in &config.providers {
        if provider_config.kind == ProviderKind::Voyageai {
            continue;
        }
        match Provider::from_config(provider_config) {
            Ok(provider) => match provider.models(provider_config).await {
                Ok(catalog) => {
                    for line in format_model_lines(name, &catalog) {
                        println!("{line}");
                    }
                }
                Err(e) => eprintln!("warning: {name}: {e}"),
            },
            Err(e) => eprintln!("warning: {name}: {e}"),
        }
    }
    Ok(())
}

fn format_model_lines(provider: &str, catalog: &[crate::providers::CatalogModel]) -> Vec<String> {
    catalog
        .iter()
        .map(|model| format!("{provider}/{}", model.id))
        .collect()
}

/// `craft stats [--sessions]`: totals plus a per-model or per-session
/// breakdown from the persistent cost ledger.
pub fn stats(sessions: bool) -> Result<()> {
    let dir = storage()?;
    let ledger = CostLedger::from_state_dir(&dir).map_err(|e| {
        InvalidSnafu {
            reason: format!("read cost ledger: {e}"),
        }
        .build()
    })?;
    let summary = ledger.summary().map_err(|e| {
        InvalidSnafu {
            reason: format!("read cost ledger: {e}"),
        }
        .build()
    })?;
    print!("{}", render_stats(&summary, ledger.path(), sessions));
    Ok(())
}

fn render_stats(summary: &CostSummary, ledger_path: &Path, sessions: bool) -> String {
    if summary.records == 0 {
        return format!(
            "No usage recorded yet (ledger: {}).\n",
            ledger_path.display()
        );
    }
    let mut out = format!(
        "Total cost:   {}\nTotal tokens: {}\nRecords:      {} across {} sessions\n\n",
        summary.display_total_cost(),
        format_tokens(summary.total_tokens),
        summary.records,
        summary.session_count(),
    );
    if sessions {
        out.push_str("Per session:\n");
        for (id, cost, tokens) in &summary.by_session {
            out.push_str(&format!(
                "  {id}  {}  {}\n",
                format_usd(*cost),
                format_tokens(*tokens)
            ));
        }
    } else {
        out.push_str("Per model:\n");
        for (model, cost, tokens) in &summary.by_model {
            out.push_str(&format!(
                "  {model}  {}  {}\n",
                format_usd(*cost),
                format_tokens(*tokens)
            ));
        }
    }
    out
}

const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const LOG_TAIL_LINES: usize = 100;
const LOG_TAIL_READ_CAP: u64 = 64 * 1024;

/// `craft doctor [--export]`: ping the current model's provider, self-heal to
/// another configured provider when it fails, and report diagnostics. The
/// report shape mirrors the reference's JSON schema.
pub async fn doctor(config: Config, export: bool) -> Result<()> {
    let dir = storage()?;
    let saved_model = model_store::read_model(&dir);

    let mut provider_status: Vec<Value> = Vec::new();
    let mut healed_to: Option<String> = None;

    // The current spec is the persisted one when its provider is configured;
    // otherwise fall back to the first configured provider's first model.
    let mut current = saved_model.as_ref().and_then(|spec| {
        let (provider, model) = spec.split_once('/')?;
        config
            .providers
            .get(provider)
            .map(|_| (provider.to_string(), model.to_string()))
    });
    if current.is_none() {
        current = first_configured_model(&config).await;
    }

    let current_ok = match &current {
        Some((name, model)) => {
            let provider_config = &config.providers[name];
            let (status, detail) = match Provider::from_config(provider_config) {
                Ok(provider) => match ping(provider.models(provider_config)).await {
                    Ok(_) => ("ok", String::new()),
                    Err(detail) => ("fail", detail),
                },
                Err(e) => ("fail", e.to_string()),
            };
            let ok = status == "ok";
            provider_status.push(json!({
                "kind": name,
                "model": format!("{name}/{model}"),
                "status": status,
                "detail": detail,
                "current": true,
            }));
            ok
        }
        None => {
            provider_status.push(json!({
                "kind": "none",
                "status": "unconfigured",
                "detail": "no model configured; run `craft setup` or set an API key",
                "current": true,
            }));
            false
        }
    };

    if !current_ok {
        for (name, provider_config) in &config.providers {
            if provider_config.kind == ProviderKind::Voyageai
                || current.as_ref().is_some_and(|(n, _)| n == name)
            {
                continue;
            }
            let provider = match Provider::from_config(provider_config) {
                Ok(p) => p,
                Err(e) => {
                    provider_status.push(json!({
                        "kind": name,
                        "status": "unavailable",
                        "detail": e.to_string(),
                    }));
                    continue;
                }
            };
            let catalog = match ping(provider.models(provider_config)).await {
                Ok(catalog) => catalog,
                Err(detail) => {
                    provider_status.push(json!({
                        "kind": name,
                        "status": "fail",
                        "detail": detail,
                    }));
                    continue;
                }
            };
            if let Some(model) = catalog.first() {
                let spec = format!("{name}/{}", model.id);
                model_store::persist_model(&dir, &spec).map_err(|e| {
                    InvalidSnafu {
                        reason: format!("persist healed model: {e}"),
                    }
                    .build()
                })?;
                healed_to = Some(spec.clone());
                provider_status.push(json!({
                    "kind": name,
                    "status": "ok",
                    "healed": true,
                    "model": spec,
                }));
                break;
            }
        }
    }

    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "config_dir": paths::config_dir().ok().map(|p| p.display().to_string()),
        "state_dir": dir.path().display().to_string(),
        "current_model": current.as_ref().map(|(n, m)| format!("{n}/{m}")),
        "saved_model": saved_model,
        "healed_to": healed_to,
        "providers": provider_status,
        "log_tail": log_tail_path().map(|p| tail_logs(&p)).unwrap_or_default(),
    });

    if export {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| InvalidSnafu {
                reason: e.to_string()
            }
            .build())?
        );
        return Ok(());
    }
    print_doctor_report(&report);
    Ok(())
}

async fn first_configured_model(config: &Config) -> Option<(String, String)> {
    for (name, provider_config) in &config.providers {
        if provider_config.kind == ProviderKind::Voyageai {
            continue;
        }
        let provider = Provider::from_config(provider_config).ok()?;
        if let Ok(catalog) = provider.models(provider_config).await
            && let Some(model) = catalog.first()
        {
            return Some((name.clone(), model.id.clone()));
        }
    }
    None
}

fn log_tail_path() -> Option<PathBuf> {
    paths::logs_dir().ok().map(|d| d.join("craft.log"))
}

/// Run a provider ping future under the doctor timeout.
async fn ping<F, T>(future: F) -> std::result::Result<T, String>
where
    F: std::future::Future<Output = crate::error::Result<T>>,
{
    match tokio::time::timeout(PING_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {}s", PING_TIMEOUT.as_secs())),
    }
}

/// The last [`LOG_TAIL_LINES`] lines of the rotating log, capped at
/// [`LOG_TAIL_READ_CAP`] bytes read.
fn tail_logs(path: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_READ_CAP);
    if start > 0 {
        let _ = file.seek(SeekFrom::Start(start));
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    let lines: Vec<&str> = buf.lines().collect();
    let begin = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines.into_iter().skip(begin).map(String::from).collect()
}

fn print_doctor_report(report: &Value) {
    println!("craft {} on {}", report["version"], report["platform"]);
    if let Some(dir) = report["config_dir"].as_str() {
        println!("config dir: {dir}");
    }
    if let Some(dir) = report["state_dir"].as_str() {
        println!("state dir:  {dir}");
    }
    println!();
    match report["current_model"].as_str() {
        Some(model) => {
            let status = report["providers"]
                .as_array()
                .and_then(|a| a.iter().find(|p| p["current"] == json!(true)))
                .and_then(|p| p["status"].as_str())
                .unwrap_or("unknown");
            let icon = if status == "ok" { "✓" } else { "✗" };
            println!("{icon} current model: {model} ({status})");
        }
        None => println!("✗ no current model configured"),
    }
    if let Some(healed) = report["healed_to"].as_str() {
        println!("✓ self-healed to: {healed}");
    }
    println!();
    println!("providers:");
    if let Some(providers) = report["providers"].as_array() {
        for p in providers {
            let kind = p["kind"].as_str().unwrap_or("?");
            let status = p["status"].as_str().unwrap_or("?");
            let detail = p["detail"].as_str().unwrap_or("");
            let marker = if p["healed"].as_bool() == Some(true) {
                " (healed)"
            } else if p["current"] == json!(true) {
                " (current)"
            } else {
                ""
            };
            if detail.is_empty() {
                println!("  {kind}: {status}{marker}");
            } else {
                println!("  {kind}: {status}{marker} — {detail}");
            }
        }
    }
    if let Some(tail) = report["log_tail"].as_array()
        && !tail.is_empty()
    {
        println!();
        println!("last {} log lines:", tail.len());
        for line in tail {
            if let Some(s) = line.as_str() {
                println!("  {s}");
            }
        }
    }
}

/// `craft prompt [system|research|general] [--plan] [--tools] [--names]`.
pub async fn prompt(variant: PromptVariant, plan: bool, tools: bool, names: bool) -> Result<()> {
    if plan && !matches!(variant, PromptVariant::System) {
        return InvalidSnafu {
            reason: "--plan can only be used with the 'system' prompt variant".to_string(),
        }
        .fail();
    }

    let cwd = std::env::current_dir().map_err(|e| {
        InvalidSnafu {
            reason: format!("resolving the current directory: {e}"),
        }
        .build()
    })?;

    if tools {
        let workspace = crate::tools::Workspace::new(&cwd).map_err(crate::error::client_error)?;
        let defs = workspace.register().definitions();
        if names {
            for def in &defs {
                println!("{}", def.name);
            }
        } else {
            let defs: Vec<Value> = defs
                .iter()
                .map(|d| {
                    json!({
                        "name": d.name,
                        "description": d.description,
                        "parameters": d.parameters,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&defs).map_err(|e| InvalidSnafu {
                    reason: e.to_string()
                }
                .build())?
            );
        }
        return Ok(());
    }

    let cwd_str = cwd.display().to_string();
    let instructions = crate::instructions::load_instructions(&cwd_str);
    let config = Config::load().await?;

    let output = match variant {
        PromptVariant::System => {
            let plan_path = plan.then(|| PathBuf::from("plan.md"));
            prompt::build_system_prompt(
                &Vars::new()
                    .set("{cwd}", &cwd_str)
                    .set("{platform}", std::env::consts::OS)
                    .set("{date}", prompt::today_utc()),
                &format!("{}{}", config.agent.preamble, instructions.text),
                &ResolvedSlots::default(),
                plan_path.as_deref(),
            )
        }
        PromptVariant::Research => prompt::assemble(
            PromptId::Research,
            &ResolvedSlots::default(),
            &instructions.text,
        ),
        PromptVariant::General => prompt::assemble(
            PromptId::General,
            &ResolvedSlots::default(),
            &instructions.text,
        ),
    };
    print!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------
// Recipes (J.5)
// ---------------------------------------------------------------------

fn discover_recipes() -> Vec<crate::skills::DiscoveredFile> {
    crate::skills::Discovery::from_env().discover_files("recipes", &["yaml", "yml", "json"])
}

/// `/craft recipe list`: print `name<TAB>description` per discovered recipe.
pub async fn recipe_list() -> Result<()> {
    let files = discover_recipes();
    if files.is_empty() {
        println!("no recipes found");
        return Ok(());
    }
    for f in &files {
        match crate::recipe::load(&f.path) {
            Ok(r) => {
                let name = r.name.as_deref().unwrap_or(&f.name);
                match &r.description {
                    Some(desc) => println!("{name}\t{desc}"),
                    None => println!("{name}"),
                }
            }
            Err(e) => eprintln!("{}: {e}", f.name),
        }
    }
    Ok(())
}

/// `craft recipe run <name>`: resolve parameters (interactive stdin for
/// missing required ones), render the template, and run the prompt
/// headless.
pub async fn recipe_run(
    name: &str,
    raw_params: &[String],
    model: Option<String>,
    output_format: crate::cli::OutputFormat,
    policy: crate::cli::PermissionPolicy,
) -> Result<()> {
    use std::io::{BufRead, Write};

    let files = discover_recipes();

    let path = files
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.path.clone())
        .or_else(|| {
            let mut matches = Vec::new();
            for f in &files {
                if let Ok(r) = crate::recipe::load(&f.path)
                    && r.name.as_deref() == Some(name)
                {
                    matches.push(f.path.clone());
                }
            }
            match matches.len() {
                1 => matches.pop(),
                0 => None,
                _ => {
                    eprintln!("recipe '{name}' is ambiguous (multiple recipes match)");
                    None
                }
            }
        })
        .ok_or_else(|| {
            InvalidSnafu {
                reason: format!("recipe '{name}' not found"),
            }
            .build()
        })?;

    let mut overrides = std::collections::HashMap::new();
    for raw in raw_params {
        let (k, v) = raw.split_once('=').ok_or_else(|| {
            InvalidSnafu {
                reason: format!("invalid --param {raw:?}, expected key=value"),
            }
            .build()
        })?;
        overrides.insert(k.trim().to_string(), v.trim().to_string());
    }

    let recipe = crate::recipe::load(&path).map_err(|e| {
        InvalidSnafu {
            reason: format!("load recipe: {e}"),
        }
        .build()
    })?;

    for param in recipe.missing_required(&overrides) {
        let label = param.description.as_deref().unwrap_or(&param.name);
        print!("{label}: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line).map_err(|e| {
            InvalidSnafu {
                reason: format!("reading parameter: {e}"),
            }
            .build()
        })?;
        let line = line.trim();
        if read == 0 || line.is_empty() {
            return InvalidSnafu {
                reason: format!(
                    "missing required recipe parameter '{}' (pass via --param {}=...)",
                    param.name, param.name
                ),
            }
            .fail();
        }
        overrides.insert(param.name.clone(), line.to_string());
    }

    let params = recipe.resolve_parameters(&overrides).map_err(|e| {
        InvalidSnafu {
            reason: format!("resolve recipe parameters: {e}"),
        }
        .build()
    })?;
    let prompt = crate::template::env_vars()
        .apply(&recipe.render(&params, &path).map_err(|e| {
            InvalidSnafu {
                reason: format!("render recipe template: {e}"),
            }
            .build()
        })?)
        .into_owned();

    let config = Config::load().await?;
    crate::cli::run_headless_query(
        config,
        crate::cli::HeadlessQuery {
            prompt,
            context: Vec::new(),
            images: Vec::new(),
            // The recipe's model field wins over the CLI flag.
            model: recipe.model.clone().or(model),
            output_format,
            verbose: false,
            mode: crate::cli::CliMode::Build,
            session_id: None,
            policy,
            // recipe run does not expose the tool flags.
            tool_policy: Vec::new(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stats::{CostRecord, CostUsage};

    #[test]
    fn format_model_lines_prefix_the_provider() {
        let catalog = vec![
            crate::providers::CatalogModel {
                id: "m1".into(),
                name: None,
                description: None,
                context_length: None,
                max_output_tokens: None,
            },
            crate::providers::CatalogModel {
                id: "m2".into(),
                name: Some("M2".into()),
                description: None,
                context_length: None,
                max_output_tokens: None,
            },
        ];
        assert_eq!(
            format_model_lines("anthropic", &catalog),
            ["anthropic/m1", "anthropic/m2"]
        );
    }

    fn rec(session: &str, model: &str, tokens: u64, cost: f64) -> CostRecord {
        CostRecord {
            session_id: session.into(),
            turn_id: None,
            ts: 0,
            model: model.into(),
            provider: "p".into(),
            usage: CostUsage {
                input: tokens,
                output: 0,
                cache_creation: 0,
                cache_read: 0,
            },
            cost_usd: Some(cost),
            fast: false,
        }
    }

    #[test]
    fn stats_rendering_matches_the_reference_layout() {
        let summary = CostSummary {
            total_cost: 1.5,
            total_tokens: 3_000_000,
            by_model: vec![("anthropic/m".into(), 1.5, 3_000_000)],
            by_session: vec![("s1".into(), 1.0, 2_000_000), ("s2".into(), 0.5, 1_000_000)],
            records: 2,
            unpriced_records: 0,
        };
        let by_model = render_stats(&summary, Path::new("/tmp/cost.jsonl"), false);
        assert!(by_model.contains("Total cost:   $1.50"));
        assert!(by_model.contains("Records:      2 across 2 sessions"));
        assert!(by_model.contains("Per model:"));
        assert!(by_model.contains("  anthropic/m  $1.50"));
        assert!(!by_model.contains("Per session:"));

        let by_session = render_stats(&summary, Path::new("/tmp/cost.jsonl"), true);
        assert!(by_session.contains("Per session:"));
        assert!(by_session.contains("  s1  $1.00"));
        assert!(by_session.contains("  s2  $0.50"));
    }

    #[test]
    fn empty_ledger_message_names_the_ledger_path() {
        let out = render_stats(&CostSummary::default(), Path::new("/x/cost.jsonl"), false);
        assert_eq!(out, "No usage recorded yet (ledger: /x/cost.jsonl).\n");
    }

    #[test]
    fn stats_prints_from_a_real_ledger_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = CostLedger::new(tmp.path());
        ledger.append(&rec("s1", "anthropic/m", 10, 0.25)).unwrap();
        let summary = ledger.summary().unwrap();
        let out = render_stats(&summary, ledger.path(), false);
        assert!(out.contains("Records:      1 across 1 sessions"));
        assert!(out.contains("  p/anthropic/m  $0.25"));
    }

    #[test]
    fn tail_logs_returns_last_n_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let content: String = (0..150).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.path().join("craft.log"), &content).unwrap();
        let tail = tail_logs(&tmp.path().join("craft.log"));
        assert_eq!(tail.len(), LOG_TAIL_LINES);
        assert!(tail[0].contains("line 50"));
    }

    #[test]
    fn tail_logs_empty_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(tail_logs(&tmp.path().join("craft.log")).is_empty());
    }
}
