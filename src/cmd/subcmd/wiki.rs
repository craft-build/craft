//! `craft wiki` subcommand: manage the in-project local wiki knowledge base
//! (`.wiki/`). Supports ingesting local files (with an LLM summary), listing
//! all pages and sources, and showing a single entry.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use craft_agent::prompt::WIKI_INIT_PROMPT;
use craft_agent::wiki::summarize;
use craft_config::load_env_files;
use craft_lua::PluginHost;
use craft_providers::provider::{self, Provider};
use craft_storage::StateDir;
use craft_storage::wiki::{
    ListingKind, SourceNote, WikiStore, extract_excerpt, first_h1_title, slugify,
};

use crate::cli::WikiAction;
use crate::cmd::headless::{self, HeadlessOptions};
use crate::print::OutputFormat;
use crate::setup;

/// Maximum bytes of source text sent to the summarization model. Bounds cost.
const MAX_SUMMARY_BYTES: usize = 24 * 1024;

pub async fn run(cmd: WikiAction) -> Result<()> {
    match cmd {
        WikiAction::Ingest { source, model } => ingest(source, model).await,
        WikiAction::List => list(),
        WikiAction::Show { id } => show(&id),
        WikiAction::Init { model } => init(model).await,
    }
}

async fn init(model_spec: Option<String>) -> Result<()> {
    let outcome = headless::run_headless(HeadlessOptions {
        model: model_spec,
        prompt: WIKI_INIT_PROMPT.to_string(),
        yolo: false,
        auto_review: false,
        no_plugins: false,
        extra_excluded_tools: vec![],
        context: vec![],
        persist_session: false,
        max_turns: None,
        allowed_tools: vec![],
        stream: true,
        mode: craft_agent::AgentMode::Build,
    })
    .await?;
    headless::print_outcome(&outcome, OutputFormat::Text);
    Ok(())
}

async fn ingest(source: PathBuf, model_spec: Option<String>) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let plugin_host = PluginHost::new(
        Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
        None,
    )
    .context("initialize lua plugin host")?;
    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;
    let discovery = craft_lua::discover_installed(false);
    let config = raw_config
        .unwrap_or_default()
        .into_config(&discovery.known_names())
        .context("invalid config")?;

    setup::init_logging(&config.storage);
    setup::install_panic_log_hook();

    let abs_source = if source.is_absolute() {
        source
    } else {
        cwd.join(&source)
    };
    let content = fs::read_to_string(&abs_source)
        .with_context(|| format!("read source file {}", abs_source.display()))?;

    let title = first_h1_title(&content).unwrap_or_else(|| {
        abs_source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string()
    });
    let slug = slugify(&title);
    let excerpt = extract_excerpt(&content);

    let model = setup::resolve_model(model_spec.as_deref(), &config.provider, &storage).await?;
    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };
    let provider: Arc<dyn Provider> = {
        let mut model_for_provider = model.clone();
        Arc::from(
            provider::from_model(&mut model_for_provider, timeouts)
                .await
                .context("init wiki provider")?,
        )
    };

    let truncated = truncate_for_summary(&content);
    let summary = summarize(provider.as_ref(), &model, &truncated, None)
        .await
        .context("summarize source")?;

    let store = WikiStore::open(&cwd).context("open wiki store")?;
    let source_str = abs_source.display().to_string();
    let ingested_at = jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let note = SourceNote {
        slug: slug.clone(),
        title: title.clone(),
        source_path: source_str,
        ingested_at,
        summary,
        excerpt,
        body: content,
        linked_pages: Vec::new(),
    };
    store
        .write_source_note(&note)
        .context("write source note")?;
    let log_message = format!("Ingested `{}` as `{}`.", abs_source.display(), slug);
    store
        .append_log(&note.ingested_at, "Creation", &log_message)
        .context("append wiki log")?;
    store.rebuild_index().context("rebuild wiki index")?;

    let note_path = store
        .root()
        .join("ingested-sources")
        .join(format!("{slug}.md"));
    println!("{}", note_path.display());
    Ok(())
}

fn truncate_for_summary(text: &str) -> String {
    if text.len() <= MAX_SUMMARY_BYTES {
        return text.to_string();
    }
    let cut = text[..MAX_SUMMARY_BYTES]
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .unwrap_or(MAX_SUMMARY_BYTES);
    text[..cut].to_string()
}

fn list() -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let store = WikiStore::open(&cwd).context("open wiki store")?;
    let listings = store.list().context("list wiki entries")?;
    if listings.is_empty() {
        println!("No wiki pages or sources yet. Use `craft wiki ingest <file>`.");
        return Ok(());
    }
    let slug_w = listings
        .iter()
        .map(|l| l.slug.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!("{:<width$}  {:<8}  TITLE", "SLUG", "KIND", width = slug_w);
    for entry in listings {
        let kind = match entry.kind {
            ListingKind::Page => "page",
            ListingKind::Source => "source",
        };
        println!(
            "{:<width$}  {:<8}  {}",
            entry.slug,
            kind,
            entry.title,
            width = slug_w
        );
    }
    Ok(())
}

fn show(id: &str) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let store = WikiStore::open(&cwd).context("open wiki store")?;
    match store.read_page(id) {
        Ok(body) => {
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        Err(craft_storage::wiki::WikiError::NotFound(_)) => {
            bail!("no wiki page or source named `{id}`");
        }
        Err(e) => Err(color_eyre::eyre::eyre!(e)),
    }
}
