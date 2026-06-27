use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::checks::{Check, ReviewOrchestrator, ReviewProgress, Severity};
use craft_agent::discovery::Discovery;
use craft_agent::types::Finding;
use regex::Regex;

use crate::cli::ReviewCommand;

pub async fn run(args: ReviewCommand) -> Result<()> {
    let discovery = Discovery::from_env();
    let check_filter = args
        .check_filter
        .as_deref()
        .map(|p| Regex::new(p).context("invalid --check-filter regex"))
        .transpose()?;
    let min_severity = args
        .severity
        .as_deref()
        .map(|s| {
            Severity::parse(s).ok_or_else(|| {
                color_eyre::eyre::eyre!("invalid --severity (use low, medium, high, or critical)")
            })
        })
        .transpose()?;

    let orchestrator = ReviewOrchestrator::new(args.model, check_filter, min_severity)
        .with_no_file_pass(args.no_file_pass);

    if args.dry_run {
        print_dry_run(&orchestrator.discover_checks(&discovery));
        return Ok(());
    }

    let outcome = orchestrator.run(&discovery, print_review_progress).await;
    for e in &outcome.errors {
        eprintln!("check '{}' failed: {}", e.check, e.message);
    }
    print_findings(&outcome.findings);
    if args.fail_on_findings && !outcome.findings.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn print_dry_run(checks: &[Check]) {
    if checks.is_empty() {
        println!("No checks discovered.");
        println!(
            "Add markdown files with frontmatter to .agents/checks/ (or ~/.config/craft/checks/)."
        );
        return;
    }
    println!("Discovered {} check(s):", checks.len());
    for c in checks {
        println!();
        println!("  {} ({})", c.name, c.path.display());
        if let Some(m) = &c.model {
            println!("    model: {m}");
        }
        if let Some(t) = c.turn_limit {
            println!("    turn-limit: {t}");
        }
        if !c.tools.is_empty() {
            println!("    tools: {}", c.tools.join(", "));
        }
        println!("    default severity: {:?}", c.severity_default);
    }
}

fn print_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("No findings.");
        return;
    }
    println!("Review: {} finding(s)", findings.len());
    for f in findings {
        println!();
        let loc = if f.line_start == f.line_end {
            format!("{}:{}", f.file_path, f.line_start)
        } else {
            format!("{}:{}-{}", f.file_path, f.line_start, f.line_end)
        };
        println!("[{}] {loc}", f.priority);
        println!("  {}", f.title);
        if !f.body.is_empty() {
            for line in f.body.lines().take(20) {
                println!("  {line}");
            }
        }
        if let Some(suggestion) = &f.suggestion {
            println!("  suggestion: {suggestion}");
        }
        if !f.rule_ids.is_empty() {
            println!("  rules: {}", f.rule_ids.join(", "));
        }
    }
}

fn print_review_progress(progress: ReviewProgress) {
    match progress {
        ReviewProgress::ChecksStarted { total } => {
            eprintln!(
                "Review: running {} check{}...",
                total,
                if total == 1 { "" } else { "s" }
            );
        }
        ReviewProgress::CheckStarted { name } => {
            eprintln!("  ▸ {}", name);
        }
        ReviewProgress::CheckFinished {
            name,
            findings,
            errored,
        } => {
            if errored {
                eprintln!("  ✗ {} (failed)", name);
            } else {
                eprintln!(
                    "  ✓ {} ({} finding{})",
                    name,
                    findings,
                    if findings == 1 { "" } else { "s" }
                );
            }
        }
        ReviewProgress::FilePassStarted { total } => {
            eprintln!(
                "Review: main pass over {} changed file{}...",
                total,
                if total == 1 { "" } else { "s" }
            );
        }
        ReviewProgress::FileReviewed {
            file,
            findings,
            errored,
        } => {
            if errored {
                eprintln!("  ✗ {} (failed)", file);
            } else {
                eprintln!(
                    "  ✓ {} ({} finding{})",
                    file,
                    findings,
                    if findings == 1 { "" } else { "s" }
                );
            }
        }
    }
}
