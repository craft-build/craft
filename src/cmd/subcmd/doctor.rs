use std::env;
use std::fs;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::Context;
use serde_json::{Value, json};
use strum::IntoEnumIterator;

use craft_config::{ProviderConfig, load_env_files};
use craft_providers::Timeouts;
use craft_providers::model::{Model, ModelTier};
use craft_providers::provider::{self, Provider, ProviderKind};
use craft_storage::StateDir;
use craft_storage::model::{persist_model, read_model};
use craft_storage::version;

use crate::setup;

const PING_TIMEOUT: Duration = Duration::from_secs(15);
const LOG_TAIL_LINES: usize = 100;
const LOG_TAIL_READ_CAP: u64 = 64 * 1024;

pub async fn run(export: bool) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let timeouts = Timeouts::default();
    let current_model = setup::resolve_model(None, &ProviderConfig::default(), &storage)
        .await
        .ok();

    let mut provider_status: Vec<Value> = Vec::new();
    let mut healed_to: Option<String> = None;

    let current_ok = if let Some(model) = &current_model {
        let mut m = model.clone();
        let ping = ping_model(&mut m, timeouts).await;
        provider_status.push(json!({
            "kind": model.provider.to_string(),
            "model": model.spec(),
            "status": ping.status(),
            "detail": ping.detail(),
            "current": true,
        }));
        ping.is_ok()
    } else {
        provider_status.push(json!({
            "kind": "none",
            "status": "unconfigured",
            "detail": "no model configured; run `craft auth login` or set an API key",
            "current": true,
        }));
        false
    };

    if !current_ok {
        for kind in ProviderKind::iter() {
            if current_model.as_ref().is_some_and(|m| m.provider == kind) {
                continue;
            }
            let provider = match kind.create(timeouts).await {
                Ok(p) => p,
                Err(e) => {
                    provider_status.push(json!({
                        "kind": kind.to_string(),
                        "status": "unavailable",
                        "detail": e.user_message(),
                    }));
                    continue;
                }
            };
            let ping = ping_provider(&*provider).await;
            if ping.is_ok()
                && let Ok(model) = Model::from_tier(kind, ModelTier::Strong)
            {
                let spec = model.spec();
                persist_model(&storage, &spec).context("persist healed model")?;
                healed_to = Some(spec.clone());
                provider_status.push(json!({
                    "kind": kind.to_string(),
                    "status": "ok",
                    "healed": true,
                    "model": spec,
                }));
                break;
            }
            provider_status.push(json!({
                "kind": kind.to_string(),
                "status": ping.status(),
                "detail": ping.detail(),
            }));
        }
    }

    let report = json!({
        "version": version::CURRENT,
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "config_dir": craft_config::global_config_dir().map(|p| p.display().to_string()),
        "state_dir": storage.path().display().to_string(),
        "current_model": current_model.as_ref().map(|m| m.spec()),
        "saved_model": read_model(&storage),
        "healed_to": healed_to,
        "providers": provider_status,
        "log_tail": tail_logs(&storage),
    });

    if export {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_human(&report);
    Ok(())
}

struct Ping {
    ok: bool,
    detail: String,
}

impl Ping {
    fn is_ok(&self) -> bool {
        self.ok
    }
    fn status(&self) -> &'static str {
        if self.ok { "ok" } else { "fail" }
    }
    fn detail(&self) -> &str {
        &self.detail
    }
}

async fn ping_model(model: &mut Model, timeouts: Timeouts) -> Ping {
    match provider::from_model(model, timeouts).await {
        Ok(p) => ping_provider(&*p).await,
        Err(e) => Ping {
            ok: false,
            detail: e.user_message(),
        },
    }
}

async fn ping_provider(provider: &dyn Provider) -> Ping {
    match tokio::time::timeout(PING_TIMEOUT, provider.list_models()).await {
        Ok(Ok(_)) => Ping {
            ok: true,
            detail: String::new(),
        },
        Ok(Err(e)) => Ping {
            ok: false,
            detail: e.user_message(),
        },
        Err(_) => Ping {
            ok: false,
            detail: format!("timed out after {}s", PING_TIMEOUT.as_secs()),
        },
    }
}

fn tail_logs(storage: &StateDir) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = storage.path().join("craft.log");
    let Ok(mut file) = fs::File::open(&path) else {
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

fn print_human(report: &Value) {
    println!("craft {} on {}", report["version"], report["platform"]);
    if let Some(dir) = report["config_dir"].as_str() {
        println!("config dir: {dir}");
    }
    if let Some(dir) = report["state_dir"].as_str() {
        println!("state dir:  {dir}");
    }
    println!();
    if let Some(model) = report["current_model"].as_str() {
        let status = report["providers"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["current"] == true))
            .and_then(|p| p["status"].as_str())
            .unwrap_or("unknown");
        let icon = if status == "ok" { "✓" } else { "✗" };
        println!("{icon} current model: {model} ({status})");
    } else {
        println!("✗ no current model configured");
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
            } else if p["current"] == true {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn tail_logs_returns_last_n_lines() {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let content: String = (0..150).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.path().join("craft.log"), &content).unwrap();
        let tail = tail_logs(&storage);
        assert_eq!(tail.len(), LOG_TAIL_LINES);
        assert!(tail[0].contains("line 50"));
    }

    #[test]
    fn tail_logs_empty_when_no_file() {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        assert!(tail_logs(&storage).is_empty());
    }
}
