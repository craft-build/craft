//! Phase 4 gate tests: the real `craft __codemode-worker` subprocess, end
//! to end through the pool. These run with the lib compiled without
//! `cfg(test)`, so the pool spawns genuine processes through
//! `CRAFT_CODEMODE_WORKER_CMD` (pointed at `CARGO_BIN_EXE_craft`).

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use animist::agent::testing::ToolHarness;
use animist::history::History;
use serde_json::json;

use craft::codemode::{Code, catalog::Catalog, diagnose::Class};
use craft::config::{CodemodeConfig, CodemodeMode};
use craft::history::ext::tool_content_text;
use craft::tools::Workspace;

const WORKER_ENV: &str = "CRAFT_CODEMODE_WORKER_CMD";

/// The pool is process-global, so every test here shares one runtime (a
/// fresh `#[tokio::test]` runtime would strand the pool's workers on a dead
/// reactor) plus a serial lock; the env override is set before the first
/// checkout.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
    // SAFETY: set before any pool use, under the serial lock; nothing else
    // in this process reads the variable.
    unsafe { std::env::set_var(WORKER_ENV, env!("CARGO_BIN_EXE_craft")) };
    guard
}

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

fn codemode_config() -> CodemodeConfig {
    CodemodeConfig {
        mode: CodemodeMode::Alongside,
        timeout_secs: 60,
        wall_clock_secs: 120,
        max_tool_calls: 64,
        max_output_kib: 32,
        ..CodemodeConfig::default()
    }
}

async fn setup(dir: &Path) -> (ToolHarness, Catalog) {
    std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
    let workspace = Workspace::new(dir)
        .unwrap()
        .with_codemode(codemode_config());
    let registry = workspace.registry(&History::new()).unwrap();
    let catalog = Catalog::from_registry(&registry);
    let harness = ToolHarness::builder().tools(registry).build().unwrap();
    (harness, catalog)
}

async fn run_code(harness: &mut ToolHarness, catalog: &Catalog, source: &str) -> String {
    let output = harness
        .call(
            Code::new(catalog.clone(), codemode_config()),
            json!({ "code": source }),
        )
        .await
        .unwrap();
    tool_content_text(&output.content)
}

fn failure_class(text: &str) -> Class {
    Class::from_rendered(text).unwrap_or_else(|| panic!("rendered failure: {text}"))
}

/// Wait until this test's run is in flight (past spawn, handshake, and the
/// run frame), then let its first tool call complete before injecting the
/// fault.
async fn wait_run_in_flight(baseline: u64) {
    while craft::codemode::debug_runs_sent() == baseline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[test]
fn subprocess_runs_gather_scripts_with_output_parity() {
    let _guard = serial();
    rt().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, catalog) = setup(dir.path()).await;
        let source = "import asyncio\nvalues = await asyncio.gather(read_text('a.txt'), read_text(path='b.txt'))\nprint(values)";

        let text = run_code(&mut harness, &catalog, source).await;

        // Output parity with the in-process unit test
        // (`scripts_dispatch_hidden_children_and_commit_shaped_output`).
        assert_eq!(text, "['alpha\\n', 'beta\\n']\n");

        // Warm reuse: a second sequential run shares the same process.
        let warm = craft::codemode::debug_idle_pids();
        let again = run_code(&mut harness, &catalog, "print('again')").await;
        assert_eq!(again, "again\n");
        let after = craft::codemode::debug_idle_pids();
        assert_eq!(
            warm, after,
            "two sequential runs reuse one warm worker process"
        );
    });
}

#[test]
fn worker_subcommand_is_hidden_from_help() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_craft"))
        .arg("--help")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("__codemode-worker"));
    assert!(text.contains("term"), "help still lists real subcommands");
}

#[test]
fn killed_worker_yields_crash_and_session_continues() {
    let _guard = serial();
    rt().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, catalog) = setup(dir.path()).await;
        // One real tool call completes (partial progress), then the
        // interpreter spins forever — monty cannot interrupt a pure loop,
        // so the worker stays mid-run until the kill lands.
        let source = "import asyncio\nprint('starting')\nvalue = await read_text('a.txt')\nspin = 0\nwhile True:\n    spin += 1";

        let baseline = craft::codemode::debug_runs_sent();
        let killer = tokio::spawn(async move {
            wait_run_in_flight(baseline).await;
            craft::codemode::debug_kill_all();
        });
        let text = run_code(&mut harness, &catalog, source).await;
        killer.await.unwrap();

        assert_eq!(
            failure_class(&text),
            Class::Crash,
            "model-facing failure is Crash: {text}"
        );
        assert!(
            text.contains("This is an engine bug, not a script error"),
            "crash message explains the blast radius: {text}"
        );

        assert!(
            text.contains("Completed 1 of 1 tool calls before failing"),
            "partial progress is preserved: {text}"
        );

        // The session continues: a subsequent code call succeeds on a fresh
        // worker (the pool replaced the killed one).
        let retry = run_code(&mut harness, &catalog, "print(await read_text('a.txt'))").await;
        assert_eq!(retry, "alpha\n\n", "session continues after the crash");
    });
}

#[test]
fn cancellation_kills_the_worker_and_the_pool_stays_warm() {
    let _guard = serial();
    rt().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, catalog) = setup(dir.path()).await;
        let source =
            "import asyncio\nawait read_text('a.txt')\nspin = 0\nwhile True:\n    spin += 1";

        let cancellation = harness.cancellation();
        let baseline = craft::codemode::debug_runs_sent();
        let cancel = tokio::spawn(async move {
            wait_run_in_flight(baseline).await;
            cancellation.cancel();
        });
        let started = Instant::now();
        let text = run_code(&mut harness, &catalog, source).await;
        let elapsed = started.elapsed();
        cancel.await.unwrap();

        assert!(
            elapsed < Duration::from_secs(10),
            "cancellation is prompt (took {elapsed:?}): {text}"
        );
        assert_eq!(failure_class(&text), Class::Cancelled, "{text}");

        // No zombie and a warm pool: the next run works. The cancelled run
        // context cannot run more code, so drive a fresh harness.
        let (mut retry_harness, retry_catalog) = setup(dir.path()).await;
        let retry = run_code(&mut retry_harness, &retry_catalog, "print('after')").await;
        assert_eq!(retry, "after\n", "pool warm again after cancellation");
    });
}

#[test]
fn recycle_cap_replaces_the_worker_process() {
    let _guard = serial();
    rt().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, catalog) = setup(dir.path()).await;
        let warm = run_code(&mut harness, &catalog, "print('warm')").await;
        assert_eq!(warm, "warm\n");
        let original = craft::codemode::debug_idle_pids();
        assert_eq!(original.len(), 1, "exactly one warm worker: {original:?}");

        // Run enough trivial scripts to cross the recycle cap; the pool then
        // hands out a fresh process.
        let mut recycled = false;
        for _ in 0..craft::codemode::RECYCLE_AFTER_RUNS + 4 {
            let text = run_code(&mut harness, &catalog, "None").await;
            assert_eq!(text, "(no output: print or end with an expression)");
            let pids = craft::codemode::debug_idle_pids();
            if pids != original {
                assert!(
                    pids.iter().any(|pid| !original.contains(pid)),
                    "a fresh worker pid {pids:?} replaced {original:?}"
                );
                recycled = true;
                break;
            }
        }
        assert!(recycled, "worker recycles after the run cap");
    });
}

/// Monty classifies parser-depth and recursion abuse rather than crashing
/// (SyntaxError "Source is too deeply nested", RecursionError), so no
/// script deterministically aborts the engine; the deterministic crash gate
/// is OS-level kill injection (test above). These probes pin the classified
/// behavior through the real subprocess.
#[test]
fn adversarial_nesting_and_recursion_are_classified_not_fatal() {
    let _guard = serial();
    rt().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, catalog) = setup(dir.path()).await;

        let depth = 200_000;
        let nested = format!("x = {}1{}", "(".repeat(depth), ")".repeat(depth));
        let text = run_code(&mut harness, &catalog, &nested).await;
        assert_eq!(
            failure_class(&text),
            Class::Syntax,
            "deep nesting is a syntax failure, not a crash: {text}"
        );

        let text = run_code(&mut harness, &catalog, "def f(): return f()\nf()").await;
        assert_eq!(
            failure_class(&text),
            Class::Limit,
            "recursion bomb is a limit failure, not a crash"
        );

        // …and the session keeps working afterwards.
        let alive = run_code(&mut harness, &catalog, "print(await read_text('a.txt'))").await;
        assert_eq!(
            alive, "alpha\n\n",
            "session continues after adversarial input"
        );
    });
}
