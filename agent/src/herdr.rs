//! Herdr host integration: report agent state to the
//! [Herdr](https://herdr.dev) terminal runtime so panes show who needs
//! attention and the current session survives a Herdr server restart.
//!
//! Two mechanisms share one [`State`] source:
//!
//! - the Herdr TUI CLI (`$HERDR_BIN_PATH pane report-agent`), gated on
//!   `HERDR_ENV=1` plus `HERDR_PANE_ID` and `HERDR_BIN_PATH`;
//! - OSC 7501 (Program Status Protocol) sequences on stdout, gated on
//!   stdout being a tty (terminals that don't know the sequence ignore
//!   it).
//!
//! Reporting is fire-and-forget by contract: failures are logged at
//! `debug` and never surfaced, reports queued while one is in flight are
//! coalesced to the latest state, and `--seq` strictly increases so
//! Herdr can drop stale reports.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SOURCE: &str = "craft";
const APP: &str = "craft";
/// One report or release may hang this long before being abandoned.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_BYTES: usize = 8 * 1024;

/// Agent state as the surfaces compute it. One vocabulary covers both
/// mechanisms; each maps it into its own wire form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Working,
    Idle,
    Blocked,
    Done,
    Error,
}

impl State {
    /// OSC 7501 field value; herdr-core stores the full vocabulary.
    fn as_osc(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Idle => "idle",
            State::Blocked => "blocked",
            State::Done => "done",
            State::Error => "error",
        }
    }

    /// Herdr TUI CLI value; that contract defines only
    /// `idle|working|blocked`, so turn ends collapse to `idle`.
    fn as_cli(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Idle | State::Done | State::Error => "idle",
            State::Blocked => "blocked",
        }
    }
}

fn osc_sequence(state: State) -> String {
    format!("\x1b]7501;state={}:app={APP}\x1b\\", state.as_osc())
}

fn arg_is_clean(arg: &str) -> bool {
    !arg.contains('\'') && !arg.chars().any(char::is_control)
}

/// The resume-command rules: plain first word on `PATH`, no apostrophes
/// or control characters in any argument, at most 64 arguments and 8 KiB
/// total. Herdr rejects the whole report on a violation
/// (`invalid_resume_argv`), so craft drops the resume part itself
/// instead and keeps the state report.
fn resume_argv_valid(argv: &[String]) -> bool {
    if argv.is_empty() || argv.len() > MAX_RESUME_ARGS {
        return false;
    }
    let plain_first_word = !argv[0].is_empty() && !argv[0].contains('/') && !argv[0].contains('\\');
    let total: usize = argv.iter().map(String::len).sum::<usize>() + argv.len() - 1;
    plain_first_word && total <= MAX_RESUME_BYTES && argv.iter().all(|a| arg_is_clean(a))
}

/// `craft --session <id>` behind `--`, or `None` when the rules above
/// would reject it.
fn build_resume_argv(session_id: &str, first_word: &str) -> Option<Vec<String>> {
    let argv = vec![
        first_word.to_string(),
        "--session".into(),
        session_id.into(),
    ];
    resume_argv_valid(&argv).then_some(argv)
}

/// First resume word: the basename of argv[0], so `~/.local/bin/craft`
/// reports the plain `craft` name the rules require; `craft` when argv[0]
/// is missing or empty.
fn first_word_of(argv0: &str) -> String {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or_default();
    if base.is_empty() {
        "craft".into()
    } else {
        base.into()
    }
}

fn argv0_first_word() -> String {
    std::env::args()
        .next()
        .as_deref()
        .map_or_else(|| "craft".into(), first_word_of)
}

/// Subcommand argv for a state report (without the binary path).
fn report_argv(
    pane: &str,
    state: State,
    message: Option<&str>,
    seq: u64,
    session: Option<&str>,
    first_word: &str,
) -> Vec<String> {
    let mut argv = vec![
        "pane".into(),
        "report-agent".into(),
        pane.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        APP.into(),
        "--state".into(),
        state.as_cli().into(),
        "--seq".into(),
        seq.to_string(),
    ];
    // `--message` exists to explain a block.
    if state == State::Blocked
        && let Some(message) = message
    {
        argv.push("--message".into());
        argv.push(message.into());
    }
    if let Some(id) = session {
        argv.push("--agent-session-id".into());
        argv.push(id.into());
        if let Some(resume) = build_resume_argv(id, first_word) {
            argv.push("--".into());
            argv.extend(resume);
        }
    }
    argv
}

fn release_argv(pane: &str, seq: u64) -> Vec<String> {
    vec![
        "pane".into(),
        "release-agent".into(),
        pane.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        APP.into(),
        "--seq".into(),
        seq.to_string(),
    ]
}

/// Runs `argv[0]` with `argv[1..]`, null stdio, killed if dropped; a
/// failure is debug-logged only. Injected so the test fake (argv
/// recorder) is the second implementation.
type SpawnFn = Arc<dyn Fn(&[String]) -> BoxFuture<'static, ()> + Send + Sync>;

fn process_spawner(argv: &[String]) -> BoxFuture<'static, ()> {
    let argv = argv.to_vec();
    async move {
        let status = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        if let Err(error) = status {
            tracing::debug!(%error, bin = %argv[0], "herdr report failed");
        }
    }
    .boxed()
}

/// Timestamp-ms sequence numbers, forced strictly increasing: two reports
/// inside one millisecond still order correctly, and Herdr drops any
/// report whose seq does not advance.
fn next_seq(seq: &AtomicU64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    loop {
        let last = seq.load(Ordering::Relaxed);
        let next = now.max(last + 1);
        if seq
            .compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// Env gate shared by [`TuiReporter::detect`] and its tests: `(bin, pane)`
/// when the process runs inside a Herdr pane. Empty values count as
/// unset.
fn detect_from(var: &dyn Fn(&str) -> Option<String>) -> Option<(String, String)> {
    if var("HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    let nonempty = |name: &str| var(name).filter(|v| !v.is_empty());
    Some((nonempty("HERDR_BIN_PATH")?, nonempty("HERDR_PANE_ID")?))
}

/// CLI reporting: one worker task drains a latest-wins watch channel, so
/// a burst of updates while a report is in flight collapses into a
/// single follow-up carrying the newest state.
struct TuiReporter {
    bin: String,
    pane: String,
    first_word: String,
    seq: AtomicU64,
    tx: watch::Sender<Vec<String>>,
    worker: JoinHandle<()>,
    spawn: SpawnFn,
}

impl Drop for TuiReporter {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl TuiReporter {
    fn new(bin: String, pane: String, first_word: String, spawn: SpawnFn) -> Self {
        let (tx, rx) = watch::channel(Vec::new());
        let worker = tokio::spawn(report_worker(rx, spawn.clone()));
        Self {
            bin,
            pane,
            first_word,
            seq: AtomicU64::new(0),
            tx,
            worker,
            spawn,
        }
    }

    fn detect() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let (bin, pane) = detect_from(&var)?;
        Some(Self::new(
            bin,
            pane,
            argv0_first_word(),
            Arc::new(process_spawner),
        ))
    }

    fn report(&self, state: State, message: Option<&str>, session: Option<&str>) {
        let seq = next_seq(&self.seq);
        let mut argv = vec![self.bin.clone()];
        argv.extend(report_argv(
            &self.pane,
            state,
            message,
            seq,
            session,
            &self.first_word,
        ));
        let _ = self.tx.send(argv);
    }

    /// Stop the worker first so an in-flight report cannot land after the
    /// release and re-register the agent.
    async fn release(&self) {
        self.worker.abort();
        let seq = next_seq(&self.seq);
        let mut argv = vec![self.bin.clone()];
        argv.extend(release_argv(&self.pane, seq));
        if timeout(REPORT_TIMEOUT, (self.spawn)(&argv)).await.is_err() {
            tracing::debug!("herdr release timed out");
        }
    }
}

async fn report_worker(mut rx: watch::Receiver<Vec<String>>, spawn: SpawnFn) {
    while rx.changed().await.is_ok() {
        let argv = rx.borrow_and_update().clone();
        if timeout(REPORT_TIMEOUT, (spawn)(&argv)).await.is_err() {
            tracing::debug!("herdr report timed out");
        }
    }
}

/// OSC sink; the test buffer is the second implementation.
type OscFn = Arc<dyn Fn(&str) + Send + Sync>;

fn stdout_osc(sequence: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

struct HerdrInner {
    osc: Option<OscFn>,
    session: Option<String>,
    last: Option<State>,
    last_message: Option<String>,
    released: bool,
}

/// Surface-facing facade over both mechanisms, built once per surface and
/// shared by `Arc` where the surface needs it from more than one place
/// (the TUI's render closure and its exit path). Every method is a no-op
/// outside Herdr and on non-tty stdout.
pub(crate) struct Herdr {
    inner: Mutex<HerdrInner>,
    reporter: Option<TuiReporter>,
}

impl Herdr {
    /// OSC on tty stdout (herdr-core consumes it; other terminals ignore
    /// unknown OSC), CLI reporting when the Herdr env gate passes.
    pub(crate) fn for_surface(stdout_is_tty: bool) -> Self {
        Self::new(
            stdout_is_tty.then(|| Arc::new(stdout_osc) as OscFn),
            TuiReporter::detect(),
        )
    }

    fn new(osc: Option<OscFn>, reporter: Option<TuiReporter>) -> Self {
        Self {
            inner: Mutex::new(HerdrInner {
                osc,
                session: None,
                last: None,
                last_message: None,
                released: false,
            }),
            reporter,
        }
    }

    /// Report `state` if it changed, or `message` if it changed while
    /// blocked (a new pending decision replaces the old explanation).
    /// The OSC write is a small synchronous write on the caller's thread
    /// — in the TUI that is the render-loop closure, so it cannot tear a
    /// frame; the CLI report is queued fire-and-forget.
    pub(crate) fn update(&self, state: State, message: Option<&str>) {
        // `--message` exists to explain a block; other states carry none.
        let message = (state == State::Blocked)
            .then_some(message)
            .flatten()
            .map(str::to_string);
        let session = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.released {
                return;
            }
            let state_changed = inner.last != Some(state);
            let message_changed =
                state == State::Blocked && inner.last_message.as_deref() != message.as_deref();
            if !state_changed && !message_changed {
                return;
            }
            inner.last = Some(state);
            inner.last_message = message.clone();
            if state_changed && let Some(osc) = &inner.osc {
                osc(&osc_sequence(state));
            }
            inner.session.clone()
        };
        if let Some(reporter) = &self.reporter {
            reporter.report(state, message.as_deref(), session.as_deref());
        }
    }

    /// Remember the session and re-send the current state so the resume
    /// command lands promptly (the contract: report the resume command
    /// again whenever the session switches; a session swap is not a
    /// release).
    pub(crate) fn set_session(&self, id: &str) {
        let (last, last_message, session) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.released || inner.session.as_deref() == Some(id) {
                return;
            }
            inner.session = Some(id.to_string());
            (
                inner.last,
                inner.last_message.clone(),
                inner.session.clone(),
            )
        };
        if let (Some(reporter), Some(state)) = (&self.reporter, last) {
            reporter.report(state, last_message.as_deref(), session.as_deref());
        }
    }

    /// Release the pane on real quit; further updates are ignored.
    pub(crate) async fn release(&self) {
        let reporter = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.released {
                return;
            }
            inner.released = true;
            self.reporter.as_ref()
        };
        if let Some(reporter) = reporter {
            reporter.release().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use futures::FutureExt;
    use tokio::sync::mpsc;

    fn recorded_spawn(tx: mpsc::UnboundedSender<Vec<String>>) -> SpawnFn {
        Arc::new(move |argv: &[String]| {
            let _ = tx.send(argv.to_vec());
            std::future::ready(()).boxed()
        })
    }

    fn buffered_osc() -> (Arc<Mutex<String>>, OscFn) {
        let buf = Arc::new(Mutex::new(String::new()));
        let sink = Arc::new({
            let buf = buf.clone();
            move |s: &str| buf.lock().unwrap_or_else(|e| e.into_inner()).push_str(s)
        });
        (buf, sink)
    }

    #[test]
    fn osc_sequence_is_the_program_status_protocol_escape() {
        assert_eq!(
            osc_sequence(State::Working),
            "\x1b]7501;state=working:app=craft\x1b\\"
        );
        assert_eq!(
            osc_sequence(State::Blocked),
            "\x1b]7501;state=blocked:app=craft\x1b\\"
        );
    }

    #[test]
    fn cli_states_collapse_done_and_error_to_idle() {
        assert_eq!(State::Done.as_cli(), "idle");
        assert_eq!(State::Error.as_cli(), "idle");
        assert_eq!(State::Idle.as_cli(), "idle");
        assert_eq!(State::Working.as_cli(), "working");
        assert_eq!(State::Blocked.as_cli(), "blocked");
        assert_eq!(State::Done.as_osc(), "done");
        assert_eq!(State::Error.as_osc(), "error");
    }

    #[test]
    fn resume_argv_rules() {
        assert_eq!(
            build_resume_argv("s1", "craft").unwrap(),
            ["craft", "--session", "s1"]
        );
        // Apostrophe / control character anywhere.
        assert!(build_resume_argv("s'1", "craft").is_none());
        assert!(build_resume_argv("s\n1", "craft").is_none());
        // First word must be a plain name, not a path.
        assert!(build_resume_argv("s1", "./craft").is_none());
        assert!(build_resume_argv("s1", "/usr/local/bin/craft").is_none());
        assert!(build_resume_argv("s1", "").is_none());
        // 8 KiB total, counting separators: the fixed argv is
        // "craft --session <id>" = 16 bytes of framing.
        assert!(build_resume_argv(&"x".repeat(8192 - 16), "craft").is_some());
        assert!(build_resume_argv(&"x".repeat(8192 - 15), "craft").is_none());
        // 64-argument cap, checked on the validator (the built shape is
        // fixed at three arguments).
        let args = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>();
        assert!(resume_argv_valid(&args(64)));
        assert!(!resume_argv_valid(&args(65)));
    }

    #[test]
    fn first_word_is_the_argv0_basename() {
        assert_eq!(first_word_of("craft"), "craft");
        assert_eq!(first_word_of("/usr/local/bin/craft"), "craft");
        assert_eq!(first_word_of("./target/debug/craft"), "craft");
        assert_eq!(first_word_of(""), "craft");
        assert_eq!(first_word_of("/"), "craft");
    }

    #[test]
    fn report_argv_carries_message_only_for_blocked_and_resume_when_valid() {
        let argv = report_argv(
            "p1",
            State::Blocked,
            Some("approve bash"),
            7,
            Some("s1"),
            "craft",
        );
        assert_eq!(
            argv,
            [
                "pane",
                "report-agent",
                "p1",
                "--source",
                "craft",
                "--agent",
                "craft",
                "--state",
                "blocked",
                "--seq",
                "7",
                "--message",
                "approve bash",
                "--agent-session-id",
                "s1",
                "--",
                "craft",
                "--session",
                "s1",
            ]
        );

        // A non-blocked state drops the message.
        let argv = report_argv("p1", State::Working, Some("m"), 8, None, "craft");
        assert!(!argv.contains(&"--message".to_string()));

        // Done maps to the CLI idle vocabulary.
        let argv = report_argv("p1", State::Done, None, 9, None, "craft");
        assert!(argv.contains(&"idle".to_string()));

        // An invalid resume argv drops the resume tail, never the report.
        let argv = report_argv("p1", State::Idle, None, 10, Some("s'1"), "craft");
        assert!(argv.windows(2).any(|w| w == ["--agent-session-id", "s'1"]));
        assert!(!argv.contains(&"--".to_string()));
    }

    #[test]
    fn release_argv_shape() {
        assert_eq!(
            release_argv("p1", 11),
            [
                "pane",
                "release-agent",
                "p1",
                "--source",
                "craft",
                "--agent",
                "craft",
                "--seq",
                "11",
            ]
        );
    }

    #[test]
    fn detect_requires_the_herdr_environment() {
        fn var(
            env: Option<&str>,
            pane: Option<&str>,
            bin: Option<&str>,
        ) -> impl Fn(&str) -> Option<String> {
            move |name: &str| match name {
                "HERDR_ENV" => env.map(str::to_string),
                "HERDR_PANE_ID" => pane.map(str::to_string),
                "HERDR_BIN_PATH" => bin.map(str::to_string),
                _ => None,
            }
        }
        let full = var(Some("1"), Some("p1"), Some("/bin/herdr"));
        assert_eq!(detect_from(&full), Some(("/bin/herdr".into(), "p1".into())));
        assert_eq!(
            detect_from(&var(Some("0"), Some("p1"), Some("/bin/herdr"))),
            None
        );
        assert_eq!(detect_from(&var(Some("1"), None, Some("/bin/herdr"))), None);
        assert_eq!(detect_from(&var(Some("1"), Some("p1"), None)), None);
        // Empty values count as unset.
        let empty = var(Some("1"), Some(""), Some("/bin/herdr"));
        assert_eq!(detect_from(&empty), None);
    }

    #[test]
    fn seq_strictly_increases_within_one_millisecond() {
        let seq = AtomicU64::new(0);
        let a = next_seq(&seq);
        let b = next_seq(&seq);
        let c = next_seq(&seq);
        assert!(a < b && b < c);
    }

    #[test]
    fn osc_writes_once_per_change_and_never_when_disabled() {
        let (buf, osc) = buffered_osc();
        let herdr = Herdr::new(Some(osc), None);
        herdr.update(State::Working, None);
        herdr.update(State::Working, None);
        herdr.update(State::Idle, None);
        assert_eq!(
            buf.lock().unwrap_or_else(|e| e.into_inner()).as_str(),
            "\x1b]7501;state=working:app=craft\x1b\\\x1b]7501;state=idle:app=craft\x1b\\"
        );

        // OSC carries no message: a refreshed block explanation emits no
        // new sequence.
        let (buf, osc) = buffered_osc();
        let herdr = Herdr::new(Some(osc), None);
        herdr.update(State::Blocked, Some("approve bash"));
        herdr.update(State::Blocked, Some("approve webfetch"));
        assert_eq!(
            buf.lock().unwrap_or_else(|e| e.into_inner()).as_str(),
            "\x1b]7501;state=blocked:app=craft\x1b\\"
        );

        let (quiet, _) = buffered_osc();
        let herdr = Herdr::new(None, None);
        herdr.update(State::Working, None);
        herdr.set_session("s1");
        assert!(quiet.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[tokio::test]
    async fn disabled_herdr_is_a_noop() {
        let herdr = Herdr::new(None, None);
        herdr.update(State::Working, None);
        herdr.set_session("s1");
        herdr.release().await;
        herdr.update(State::Idle, None);
    }

    /// The scripted-session flow: idle + resume at startup, working on
    /// submit, blocked with a message, idle on turn end, release on quit.
    #[tokio::test]
    async fn reports_flow_with_resume_and_release() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = TuiReporter::new(
            "/fake/herdr".into(),
            "p1".into(),
            "craft".into(),
            recorded_spawn(tx),
        );
        let herdr = Herdr::new(None, Some(reporter));
        let seq_of = |argv: &[String]| {
            argv.windows(2)
                .find(|w| w[0] == "--seq")
                .map(|w| w[1].parse::<u64>().unwrap())
                .unwrap()
        };
        let mut seqs = Vec::new();

        herdr.set_session("s1");
        herdr.update(State::Idle, None);
        let argv = rx.recv().await.unwrap();
        seqs.push(seq_of(&argv));
        assert_eq!(&argv[..3], ["/fake/herdr", "pane", "report-agent"]);
        assert!(argv.contains(&"idle".to_string()));
        assert!(argv.windows(3).any(|w| w == ["--", "craft", "--session"]));
        assert!(argv.windows(2).any(|w| w == ["--agent-session-id", "s1"]));

        herdr.update(State::Working, None);
        let argv = rx.recv().await.unwrap();
        seqs.push(seq_of(&argv));
        assert!(argv.contains(&"working".to_string()));

        herdr.update(State::Blocked, Some("approve bash"));
        let argv = rx.recv().await.unwrap();
        seqs.push(seq_of(&argv));
        assert!(argv.windows(2).any(|w| w == ["--state", "blocked"]));
        assert!(argv.windows(2).any(|w| w == ["--message", "approve bash"]));

        herdr.update(State::Blocked, Some("still blocked"));
        herdr.update(State::Idle, None);
        let argv = rx.recv().await.unwrap();
        seqs.push(seq_of(&argv));
        assert!(argv.windows(2).any(|w| w == ["--state", "idle"]));

        herdr.release().await;
        let argv = rx.recv().await.unwrap();
        seqs.push(seq_of(&argv));
        assert_eq!(&argv[1..3], ["pane", "release-agent"]);
        assert!(rx.try_recv().is_err());

        // seq strictly increases across the whole flow, release included.
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[tokio::test]
    async fn bursts_coalesce_to_the_latest_state() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gate = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let spawn: SpawnFn = {
            let gate = Arc::clone(&gate);
            let calls = Arc::clone(&calls);
            Arc::new(move |argv: &[String]| {
                let _ = tx.send(argv.to_vec());
                calls.fetch_add(1, Ordering::SeqCst);
                let gate = Arc::clone(&gate);
                async move {
                    gate.notified().await;
                }
                .boxed()
            })
        };
        let reporter = TuiReporter::new("/fake/herdr".into(), "p1".into(), "craft".into(), spawn);
        let herdr = Herdr::new(None, Some(reporter));

        herdr.update(State::Working, None);
        let first = rx.recv().await.unwrap();
        assert!(first.contains(&"working".to_string()));

        // Burst while the first report is still in flight.
        herdr.update(State::Blocked, None);
        herdr.update(State::Idle, None);
        herdr.update(State::Error, None);
        gate.notify_one();

        let second = rx.recv().await.unwrap();
        // Error collapses to the CLI idle vocabulary; the point is that
        // only the latest queued state produced a follow-up.
        assert!(second.windows(2).any(|w| w == ["--state", "idle"]));

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(rx.try_recv().is_err());
    }

    /// A session switch mid-run re-reports the current state so the new
    /// resume command lands (and does not release the pane).
    #[tokio::test]
    async fn a_new_block_message_rereports_without_a_state_change() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = TuiReporter::new(
            "/fake/herdr".into(),
            "p1".into(),
            "craft".into(),
            recorded_spawn(tx),
        );
        let herdr = Herdr::new(None, Some(reporter));

        herdr.update(State::Blocked, Some("approve bash"));
        let argv = rx.recv().await.unwrap();
        assert!(argv.windows(2).any(|w| w == ["--message", "approve bash"]));

        // A second decision replaces the explanation while staying blocked.
        herdr.update(State::Blocked, Some("approve webfetch"));
        let argv = rx.recv().await.unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w == ["--message", "approve webfetch"])
        );

        // The same message does not re-report.
        herdr.update(State::Blocked, Some("approve webfetch"));
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_switch_rereports_without_release() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = TuiReporter::new(
            "/fake/herdr".into(),
            "p1".into(),
            "craft".into(),
            recorded_spawn(tx),
        );
        let herdr = Herdr::new(None, Some(reporter));

        herdr.set_session("s1");
        herdr.update(State::Idle, None);
        let argv = rx.recv().await.unwrap();
        assert!(argv.windows(2).any(|w| w == ["--agent-session-id", "s1"]));

        herdr.set_session("s2");
        let argv = rx.recv().await.unwrap();
        assert!(argv.windows(2).any(|w| w == ["--agent-session-id", "s2"]));
        assert!(argv.windows(3).any(|w| w == ["--", "craft", "--session"]));
        assert!(!argv.contains(&"release-agent".to_string()));
    }
}
