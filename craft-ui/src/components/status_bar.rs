use std::borrow::Cow;
use std::env;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{RetryInfo, Status};

use crate::animation::spinner_frame;
use crate::repaint::{Cadence, Dirty};
use crate::theme;

use craft_providers::format_tokens;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const FAST_LABEL: &str = " [fast]";
const YOLO_LABEL: &str = " [yolo]";
const TRUNCATE_PREFIX: &str = "..";

const CONTEXT_BAR_WIDTH: usize = 10;
const BAR_FILL: &str = "█";
const BAR_EMPTY: &str = "░";

fn context_bar(pct: u32) -> (String, String) {
    let filled =
        (((pct as f32 / 100.0) * CONTEXT_BAR_WIDTH as f32).round() as usize).min(CONTEXT_BAR_WIDTH);
    let empty = CONTEXT_BAR_WIDTH - filled;
    (BAR_FILL.repeat(filled), BAR_EMPTY.repeat(empty))
}

pub struct UsageStats {
    pub context_size: u32,
    pub cost: Option<f64>,
    pub global_cost: Option<f64>,
    pub context_window: u32,
    pub show_global: bool,
}

pub struct StatusBarContext<'a> {
    pub status: &'a Status,
    pub mode_label: Cow<'static, str>,
    pub mode_style: Style,
    pub model_id: &'a str,
    pub stats: UsageStats,
    pub auto_scroll: bool,
    pub chat_name: Option<&'a str>,
    pub retry_info: Option<&'a RetryInfo>,
    pub thinking_label: Option<Cow<'static, str>>,
    pub fast: bool,
    pub yolo: bool,
    pub restoring: bool,
}

pub struct StatusBar {
    flash: Option<(String, Instant)>,
    started_at: Instant,
    cwd_branch: String,
    pub flash_duration: Duration,
    branch_update_rx: Option<flume::Receiver<()>>,
}

impl StatusBar {
    pub fn new(flash_duration: Duration) -> Self {
        Self {
            flash: None,
            started_at: Instant::now(),
            cwd_branch: cwd_branch_label(),
            flash_duration,
            branch_update_rx: spawn_branch_watcher(),
        }
    }

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    #[cfg(test)]
    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(s, _)| s.as_str())
    }

    pub fn refresh_cwd(&mut self) {
        self.cwd_branch = cwd_branch_label();
    }

    pub fn poll_branch_update(&mut self) -> Dirty {
        let Some(rx) = &self.branch_update_rx else {
            return Dirty::NO;
        };
        if rx.try_iter().next().is_none() {
            return Dirty::NO;
        }
        let branch = cwd_branch_label();
        let changed = branch != self.cwd_branch;
        self.cwd_branch = branch;
        Dirty::from(changed)
    }

    pub fn clear_flash(&mut self) {
        self.flash = None;
    }

    pub fn clear_expired_hint(&mut self) -> Dirty {
        if self
            .flash
            .as_ref()
            .is_none_or(|(_, t)| t.elapsed() < self.flash_duration)
        {
            return Dirty::NO;
        }
        self.flash = None;
        Dirty::YES
    }

    /// The bar spins for a whole turn, again while a restore is in flight, and
    /// it counts a retry down by the second. It sits next to [`Self::view`] so
    /// a new moving span cannot forget to claim its frames.
    pub fn cadence(status: &Status, restoring: bool, retrying: bool) -> Cadence {
        Cadence::when(
            *status == Status::Streaming || restoring || retrying,
            Cadence::SPINNER,
        )
    }

    pub fn view(&self, frame: &mut Frame, area: Rect, ctx: &StatusBarContext) {
        let t = theme::current();
        let bg = if t.status_bg != Color::Reset {
            t.status_bg
        } else {
            t.layer02
        };
        frame.render_widget(Block::default().style(Style::new().bg(bg)), area);
        let mut left_spans = Vec::new();

        if *ctx.status == Status::Streaming {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            left_spans.push(Span::styled(format!(" {ch}"), theme::current().spinner));
        }

        if ctx.restoring {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            left_spans.push(Span::styled(
                format!(" {ch}"),
                theme::current().status_notice,
            ));
        }

        left_spans.push(Span::styled(format!(" {}", ctx.mode_label), ctx.mode_style));

        if let Some(name) = ctx.chat_name {
            left_spans.push(Span::styled(
                format!(" [{name}]"),
                theme::current().status_dim,
            ));
        }

        if !ctx.auto_scroll {
            left_spans.push(Span::styled(
                " auto-scroll paused",
                theme::current().status_dim,
            ));
        }

        if let Some(retry) = ctx.retry_info {
            let secs = retry
                .deadline
                .saturating_duration_since(Instant::now())
                .as_secs();
            left_spans.push(Span::styled(
                format!(" {}", retry.message),
                theme::current().status_retry_error,
            ));
            left_spans.push(Span::styled(
                format!(" · retrying in {secs}s (#{})", retry.attempt),
                theme::current().status_retry_info,
            ));
        }

        let mut right_spans = Vec::new();

        match ctx.status {
            Status::Error { message: e, .. } => {
                left_spans.push(Span::styled(format!(" {e}"), theme::current().error));
            }
            _ => {
                let left_width = left_spans.iter().map(Span::width).sum::<usize>() as u16;
                let cwd =
                    truncate_tail(&self.cwd_branch, area.width.saturating_sub(left_width + 1));
                right_spans.push(Span::styled(
                    cwd,
                    Style::new().fg(theme::current().text_helper),
                ));
                right_spans.push(Span::raw(" "));
            }
        }

        if let Some((ref msg, _)) = self.flash {
            left_spans.push(Span::styled(
                format!(" {msg}"),
                theme::current().status_notice,
            ));
        }

        let right_width = right_spans.iter().map(|s| s.width() as u16).sum();
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
            right_area,
        );
    }
}

/// Total height reserved for [`view_model_row`], including its top padding.
pub const MODEL_ROW_HEIGHT: u16 = 2;
const MODEL_ROW_PAD_LEFT: u16 = 2;
const MODEL_ROW_PAD_RIGHT: u16 = 1;
const MODEL_ROW_PAD_TOP: u16 = 1;

/// Model/usage row shown above the input box: model id and thinking/fast
/// annotations on the left, context usage bar and cost right-aligned. Moved
/// out of the bottom status bar so that bar can stay focused on mode/branch
/// information.
pub fn view_model_row(frame: &mut Frame, area: Rect, ctx: &StatusBarContext) {
    let t = theme::current();
    frame.render_widget(Block::default().style(Style::new().bg(t.layer01)), area);

    if area.height <= MODEL_ROW_PAD_TOP
        || area.width <= MODEL_ROW_PAD_LEFT + MODEL_ROW_PAD_RIGHT + 4
        || matches!(ctx.status, Status::Error { .. })
    {
        return;
    }

    let content = Rect {
        x: area.x + MODEL_ROW_PAD_LEFT,
        y: area.y + MODEL_ROW_PAD_TOP,
        width: area.width - MODEL_ROW_PAD_LEFT - MODEL_ROW_PAD_RIGHT,
        height: 1,
    };

    let pct = if ctx.stats.context_window > 0 {
        (ctx.stats.context_size as f64 / ctx.stats.context_window as f64 * 100.0) as u32
    } else {
        0
    };

    let mut left_spans = vec![Span::styled(
        ctx.model_id.to_string(),
        Style::new().fg(t.text_primary).bold(),
    )];

    if let Some(ref label) = ctx.thinking_label {
        left_spans.push(Span::styled(format!(" [{label}]"), t.status_dim));
    }

    if ctx.fast {
        left_spans.push(Span::styled(FAST_LABEL, t.status_dim));
    }

    let mut right_spans = Vec::new();
    if ctx.yolo {
        right_spans.push(Span::styled(YOLO_LABEL, theme::current().error));
    }
    right_spans.push(Span::styled(
        format!(
            "{}/{} ({}%) ",
            format_tokens(ctx.stats.context_size),
            format_tokens(ctx.stats.context_window),
            pct,
        ),
        Style::new().fg(t.text_secondary),
    ));
    let (filled, empty) = context_bar(pct);
    right_spans.push(Span::styled(filled, t.accent));
    right_spans.push(Span::styled(empty, t.status_dim));

    if let Some(cost) = ctx.stats.cost {
        right_spans.push(Span::styled(
            format!(" ${cost:.3}"),
            Style::new().fg(t.text_secondary),
        ));
        if ctx.stats.show_global
            && let Some(global_cost) = ctx.stats.global_cost
        {
            right_spans.push(Span::styled(
                format!(" \u{03a3}${global_cost:.3}"),
                Style::new().fg(t.text_secondary),
            ));
        }
    }

    let right_width: u16 = right_spans.iter().map(|s| s.width() as u16).sum();
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(content);

    frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
        right_area,
    );
}

fn collapse_home(path: &str) -> String {
    let Some(home) = craft_storage::paths::home() else {
        return path.to_string();
    };
    collapse_home_with(path, &home.to_string_lossy())
}

fn collapse_home_with(path: &str, home: &str) -> String {
    path.strip_prefix(home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn truncate_tail(s: &str, max_width: u16) -> String {
    let max = max_width as usize;
    if s.width() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(TRUNCATE_PREFIX.width());
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    format!("{TRUNCATE_PREFIX}{}", &s[start..])
}

fn cwd_branch_label() -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let label = collapse_home(&cwd);
    match detect_branch(&cwd) {
        Some(branch) => format!("{label}:{branch}"),
        None => label,
    }
}

fn detect_branch(cwd: &str) -> Option<String> {
    let head = std::fs::read_to_string(find_git_dir(Path::new(cwd))?.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.get(..7)?.to_string()))
}

fn find_git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = cwd;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        dir = dir.parent()?;
    }
}

fn spawn_branch_watcher() -> Option<flume::Receiver<()>> {
    use notify::{RecursiveMode, Watcher};

    let cwd = env::current_dir().ok()?;
    let git_dir = find_git_dir(&cwd)?;
    let (tx, rx) = flume::bounded(1);

    std::thread::spawn(move || {
        let Ok(mut watcher) = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if res.is_ok_and(|e| e.paths.iter().any(|p| p.ends_with("HEAD"))) {
                let _ = tx.try_send(());
            }
        }) else {
            return;
        };
        if watcher.watch(&git_dir, RecursiveMode::NonRecursive).is_ok() {
            std::thread::park();
        }
    });

    Some(rx)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::repaint::expect::QUIET;
    use tempfile::TempDir;
    use test_case::test_case;

    const FLASH_TTL: Duration = Duration::from_secs(3600);
    const FLASH_MSG: &str = "Copied";
    const STALE_BRANCH: &str = "/nowhere:gone";

    #[test_case("/home/user/projects/app", "/home/user", "~/projects/app" ; "inside_home")]
    #[test_case("/tmp/other", "/home/user", "/tmp/other"                  ; "outside_home")]
    #[test_case("/home/user", "/home/user", "~"                           ; "exact_home")]
    fn collapse_home_cases(path: &str, home: &str, expected: &str) {
        assert_eq!(collapse_home_with(path, home), expected);
    }

    fn tmp_with_head(content: Option<&str>) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        if let Some(head) = content {
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("HEAD"), head).unwrap();
        }
        let path = dir.path().to_string_lossy().into_owned();
        (dir, path)
    }

    #[test_case(Some("ref: refs/heads/feature/foo\n"), Some("feature/foo") ; "regular_ref")]
    #[test_case(Some("abc1234deadbeef\n"),            Some("abc1234")      ; "detached_head")]
    #[test_case(None,                                 None                 ; "no_git_dir")]
    fn detect_branch_cases(head: Option<&str>, expected: Option<&str>) {
        let (_dir, path) = tmp_with_head(head);
        assert_eq!(detect_branch(&path), expected.map(String::from));
    }

    #[test]
    fn detect_branch_from_subdirectory() {
        let (_dir, path) = tmp_with_head(Some("ref: refs/heads/main\n"));
        let sub = Path::new(&path).join("sub");
        fs::create_dir(&sub).unwrap();
        assert_eq!(
            detect_branch(&sub.to_string_lossy()),
            Some("main".to_string())
        );
    }

    /// Once the flash is gone nothing clears the debt, so only the tick that
    /// removes it may report a change, or the loop never settles. The two
    /// lifetimes stand in for time passing: rewinding an `Instant` by an hour
    /// panics on a machine that booted less than an hour ago.
    #[test_case(false, FLASH_TTL      => Dirty::NO  ; "no_flash")]
    #[test_case(true,  FLASH_TTL      => Dirty::NO  ; "flash_still_visible")]
    #[test_case(true,  Duration::ZERO => Dirty::YES ; "flash_expired")]
    fn clear_expired_hint_owes_the_frame_only_once(flashing: bool, ttl: Duration) -> Dirty {
        let mut bar = StatusBar::new(ttl);
        if flashing {
            bar.flash(FLASH_MSG.into());
        }

        let first = bar.clear_expired_hint();
        assert_eq!(bar.clear_expired_hint(), Dirty::NO, "{QUIET}");
        first
    }

    /// The watcher fires for any write near `.git/HEAD`, most of which leave
    /// the branch alone, so repainting on each one means a repaint per commit,
    /// stash and index refresh while a build touches the repo. Either way the
    /// poll has to leave the bounded channel empty, or the watcher's
    /// `try_send` drops the next real switch.
    #[test_case(false => Dirty::NO  ; "unchanged_branch")]
    #[test_case(true  => Dirty::YES ; "switched_branch")]
    fn poll_branch_update_reports_only_real_changes(stale: bool) -> Dirty {
        let label = cwd_branch_label();
        let (tx, rx) = flume::bounded(1);
        let mut bar = StatusBar::new(FLASH_TTL);
        bar.cwd_branch = if stale {
            STALE_BRANCH.into()
        } else {
            label.clone()
        };
        bar.branch_update_rx = Some(rx);
        tx.send(()).unwrap();

        let dirty = bar.poll_branch_update();
        assert_eq!(bar.cwd_branch, label);
        assert!(
            tx.try_send(()).is_ok(),
            "a full channel makes the watcher drop the next switch"
        );
        dirty
    }

    #[test]
    fn clear_flash_removes_flash() {
        let mut bar = StatusBar::new(Duration::from_secs(999));
        bar.flash("Copied".into());
        bar.clear_flash();
        assert!(bar.flash.is_none());
    }

    #[test_case(0,   (0, 10)  ; "empty")]
    #[test_case(50,  (5, 5)   ; "half")]
    #[test_case(100, (10, 0)  ; "full")]
    #[test_case(150, (10, 0)  ; "over_full_clamps")]
    fn context_bar_fills(pct: u32, expected: (usize, usize)) {
        let (filled, empty) = context_bar(pct);
        assert_eq!(filled.chars().count(), expected.0);
        assert_eq!(empty.chars().count(), expected.1);
    }

    /// Yolo now outlives the process that turned it on, so the one-shot flash
    /// is no longer enough to tell the user their prompts are being skipped.
    #[test_case(true  => true  ; "a_bypassed_session_says_so")]
    #[test_case(false => false ; "a_prompting_session_stays_quiet")]
    fn the_model_row_advertises_yolo(yolo: bool) -> bool {
        const BAR_WIDTH: u16 = 80;
        let ctx = StatusBarContext {
            status: &Status::Idle,
            mode_label: "normal".into(),
            mode_style: Style::new(),
            model_id: "test-model",
            stats: UsageStats {
                context_size: 100,
                cost: None,
                global_cost: None,
                context_window: 1000,
                show_global: false,
            },
            auto_scroll: true,
            chat_name: None,
            retry_info: None,
            thinking_label: None,
            fast: false,
            yolo,
            restoring: false,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(BAR_WIDTH, 4)).unwrap();
        terminal
            .draw(|f| view_model_row(f, f.area(), &ctx))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains(YOLO_LABEL.trim())
    }

    #[test_case("~/projects/craft:main", 30, "~/projects/craft:main" ; "fits_untouched")]
    #[test_case("~/projects/craft:main", 10, "..aft:main"            ; "ascii_tail")]
    #[test_case("~/文档/proj:分支", 8, "..j:分支"                    ; "cjk_path_and_branch")]
    #[test_case("release/🚀-v2", 6, "..-v2"                          ; "emoji_branch")]
    #[test_case("abc", 2, ".."                                       ; "prefix_only")]
    #[test_case("", 0, ""                                           ; "empty")]
    fn truncate_tail_cases(input: &str, max_width: u16, expected: &str) {
        assert_eq!(truncate_tail(input, max_width), expected);
    }
}
