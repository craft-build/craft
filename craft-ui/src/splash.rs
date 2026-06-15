use crate::components::keybindings::key;
use crate::theme::{self, lerp_u8};
use crate::update;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::time::Instant;

const LOGO: &str = "craft";
const TAGLINE: &str = "the efficient coder";
const HELP_SEGMENTS: &[(&str, bool)] = &[
    (key::HELP.label, true),
    (" help", false),
    (" · ", false),
    ("/help", true),
    (" in chat", false),
];

const TIPS: &[(&str, &str)] = &[
    (
        key::FILE_PICKER.label,
        "to grab file paths with fuzzy search",
    ),
    (key::TASKS.label, "to see what your subagents are up to"),
    (key::SEARCH.label, "to find things in the conversation"),
    ("/btw", "to ask something without interrupting the session"),
    ("/memory", "to view, edit, and delete persistent notes"),
    ("/cd", "to switch to a different directory"),
];

const COLOR_TRANSITION_SECS: f32 = 0.4;

/// Seconds for the initial fade-in animation (ease-out cubic).
const FADE_DURATION: f32 = 1.6;
/// Seconds to wait before the logo starts appearing.
const LOGO_DELAY: f32 = 0.2;
/// Seconds over which the logo fades from dim to full brightness.
const LOGO_RAMP: f32 = 0.8;

// ── Wiring background ────────────────────────────────────────────────────────
// A field of Manhattan "circuit traces" that run straight, snap at 90° corners,
// and carry a moving signal pulse — echoing the banner's PCB-trace aesthetic.

/// Banner crimson — the primary signal color.
const WIRE_CRIMSON: (u8, u8, u8) = (230, 18, 76);
/// Banner cyan — the secondary signal color.
const WIRE_CYAN: (u8, u8, u8) = (0, 224, 224);

/// One trace per this many cells (area.width * area.height), clamped below.
const TRACE_DENSITY_DIVISOR: usize = 75;
const TRACE_MIN: usize = 30;
const TRACE_MAX: usize = 90;
/// Straight-run length between corners (cells), inclusive-exclusive range.
const RUN_MIN: i32 = 4;
const RUN_MAX: i32 = 14;
/// Number of straight segments (corners + 1) per trace.
const SEG_MIN: i32 = 4;
const SEG_MAX: i32 = 9;
/// Slight lean toward Down/Right at corners — keeps a hint of the banner's
/// diagonal flow without skewing overall coverage to one side.
const DRIFT_BIAS: f32 = 1.00;

/// Resting brightness of an idle trace (blend toward the wire color).
const BASE_ALPHA: f32 = 0.08;
/// Extra brightness added at the signal pulse.
const PULSE_GAIN: f32 = 0.55;
/// Hard ceiling so the wiring never competes with the logo.
const ALPHA_MAX: f32 = 0.7;
/// Pulse travel speed in cells per second.
const PULSE_SPEED: f32 = 13.0;
/// Dark gap appended to each trace so pulses arrive as discrete signals.
const PULSE_GAP: f32 = 14.0;
/// Length scales of the comet: bright head, dim trailing wake, faint lead glow.
const PULSE_HEAD: f32 = 2.5;
const PULSE_TAIL: f32 = 9.0;
const TAIL_GAIN: f32 = 0.45;
const PULSE_LEAD: f32 = 3.0;
const LEAD_GAIN: f32 = 0.25;
/// Radial edge fade. Higher = tighter spotlight, darker corners.
const EDGE_FADE: f32 = 0.45;
/// Dim the central rows so the logo/tagline/help sit on a calm backdrop.
const CENTER_HALF_BAND: f32 = 4.0;
const CENTER_MIN_DIM: f32 = 0.45;

/// Direction unit vectors, indexed Right, Down, Left, Up.
const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

#[inline(always)]
fn opposite(dir: usize) -> usize {
    (dir + 2) & 3
}

/// Pick the box-drawing glyph for a cell connecting two of its four sides.
#[inline]
fn corner_glyph(side_a: usize, side_b: usize) -> char {
    match (1u8 << side_a) | (1u8 << side_b) {
        0b0101 => '─', // Left + Right
        0b1010 => '│', // Up + Down
        0b0011 => '┌', // Right + Down
        0b0110 => '┐', // Left + Down
        0b1001 => '└', // Right + Up
        0b1100 => '┘', // Left + Up
        _ => '·',
    }
}

/// Choose a 90° turn from `dir`, biased toward Down/Right so traces drift
/// across the screen like the banner's diagonal composition.
#[inline]
fn turn(dir: usize, rng: &mut Rng) -> usize {
    let a = (dir + 1) & 3;
    let b = (dir + 3) & 3;
    let a_drift = matches!(a, 0 | 1);
    let b_drift = matches!(b, 0 | 1);
    let preferred = if a_drift && !b_drift {
        a
    } else if b_drift && !a_drift {
        b
    } else if rng.next_u32() & 1 == 0 {
        a
    } else {
        b
    };
    if rng.next_f32() < DRIFT_BIAS {
        preferred
    } else if preferred == a {
        b
    } else {
        a
    }
}

/// Minimal deterministic xorshift32 — keeps trace layout stable across frames
/// (only the pulse moves) without pulling in an rng dependency.
struct Rng(u32);

impl Rng {
    #[inline]
    fn new(seed: u32) -> Self {
        Self(seed | 1)
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    #[inline]
    fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Inclusive low, exclusive high. Caller ensures `hi > lo`.
    #[inline]
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u32() % (hi - lo) as u32) as i32
    }
}

pub struct ColorTransition {
    from: (u8, u8, u8),
    to: (u8, u8, u8),
    start: Instant,
}

impl ColorTransition {
    pub fn new(color: Color) -> Self {
        let rgb = extract_rgb(color, (100, 140, 255));
        Self {
            from: rgb,
            to: rgb,
            start: Instant::now() - std::time::Duration::from_secs_f32(COLOR_TRANSITION_SECS),
        }
    }

    pub fn set(&mut self, color: Color) {
        let rgb = extract_rgb(color, (100, 140, 255));
        if rgb == self.to {
            return;
        }
        let now = Instant::now();
        self.from = self.resolve_rgb(now);
        self.to = rgb;
        self.start = now;
    }

    pub fn is_animating(&self) -> bool {
        Instant::now().duration_since(self.start).as_secs_f32() < COLOR_TRANSITION_SECS
    }

    pub fn resolve(&self) -> Color {
        let (r, g, b) = self.resolve_rgb(Instant::now());
        Color::Rgb(r, g, b)
    }

    fn resolve_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let t = (now.duration_since(self.start).as_secs_f32() / COLOR_TRANSITION_SECS).min(1.0);
        let p = ease_out_cubic(t);
        (
            lerp_u8(self.from.0, self.to.0, p),
            lerp_u8(self.from.1, self.to.1, p),
            lerp_u8(self.from.2, self.to.2, p),
        )
    }
}

pub struct Splash {
    start: Instant,
    field_offset: f32,
    seed: u32,
    animate: bool,
    tip_idx: usize,
}

impl Default for Splash {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Splash {
    pub fn new(animate: bool) -> Self {
        let mut rng = [0u8; 8];
        getrandom::fill(&mut rng).ok();
        let tip_idx = u32::from_le_bytes([rng[4], rng[5], rng[6], rng[7]]) as usize % TIPS.len();
        Self {
            start: Instant::now(),
            field_offset: (u64::from_le_bytes(rng) % 10_000) as f32,
            seed: u32::from_le_bytes([rng[0], rng[1], rng[2], rng[3]]),
            animate,
            tip_idx,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, accent: Color) {
        if area.width < 20 || area.height < 5 {
            return;
        }

        let t = self.start.elapsed().as_secs_f32();
        let fade = if t >= FADE_DURATION {
            1.0
        } else {
            ease_out_cubic(t / FADE_DURATION)
        };

        let new_version = update::latest_version();
        let block_height = 8;
        let top_y = area.y + area.height.saturating_sub(block_height) / 2;
        let tag_y = top_y + 1;
        let help_y = tag_y + 2;
        let tip_y = help_y + 2;

        if self.animate {
            self.render_wiring(area, buf, t + self.field_offset, fade, accent);
        }
        self.render_logo(area, buf, t, fade, top_y, accent);
        render_centered_faded(area, buf, fade, 0.75, tag_y, TAGLINE);
        self.render_help(area, buf, fade, help_y, accent);
        self.render_tip(area, buf, fade, tip_y, accent);
        render_version(area, buf, fade, area.y, new_version);
    }

    fn render_wiring(&self, area: Rect, buf: &mut Buffer, clock: f32, fade: f32, accent: Color) {
        let theme = theme::current();
        let bg = extract_rgb(theme.background, (15, 15, 25));
        let accent_rgb = extract_rgb(accent, (100, 140, 255));
        // Three signal colors: the two banner accents plus the active theme accent,
        // so the wiring echoes the banner while still tracking the user's theme.
        let palette = [WIRE_CRIMSON, WIRE_CYAN, accent_rgb];

        let w = area.width as i32;
        let h = area.height as i32;
        if w < 2 || h < 2 {
            return;
        }
        let inv_w = 1.0 / w as f32;
        let inv_h = 1.0 / h as f32;
        let center_y = h as f32 * 0.5;

        let trace_count = ((w * h) as usize / TRACE_DENSITY_DIVISOR).clamp(TRACE_MIN, TRACE_MAX);

        // Reused across traces to avoid per-frame allocation churn.
        let mut pts: Vec<(i32, i32)> = Vec::with_capacity((w + h) as usize);
        let mut step_dir: Vec<usize> = Vec::with_capacity((w + h) as usize);

        for ti in 0..trace_count {
            // Seed from the per-instance layout seed only (not the clock), so wires
            // stay put frame to frame and just the pulse slides along them.
            let mut rng = Rng::new(self.seed ^ (ti as u32).wrapping_mul(0x9E37_79B9));
            let color = palette[ti % palette.len()];
            let phase = rng.next_f32();

            // Start anywhere in the field, heading in any direction, so traces
            // spread evenly instead of piling up along the entry edges.
            let mut x = rng.range(0, w);
            let mut y = rng.range(0, h);
            let mut dir = (rng.next_u32() & 3) as usize;

            pts.clear();
            step_dir.clear();
            pts.push((x, y));

            let segments = rng.range(SEG_MIN, SEG_MAX);
            'walk: for seg in 0..segments {
                let run = rng.range(RUN_MIN, RUN_MAX);
                for _ in 0..run {
                    let (dx, dy) = DIRS[dir];
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || nx >= w || ny < 0 || ny >= h {
                        break 'walk;
                    }
                    x = nx;
                    y = ny;
                    step_dir.push(dir);
                    pts.push((x, y));
                }
                if seg + 1 < segments {
                    dir = turn(dir, &mut rng);
                }
            }

            let len = pts.len();
            if len < 3 {
                continue;
            }
            let path_len = len as f32;
            let period = path_len + PULSE_GAP;
            // Pulse head position along the path, wrapping through the dark gap.
            let head_pos = (clock * PULSE_SPEED + phase * period).rem_euclid(period);

            for (k, &(px, py)) in pts.iter().enumerate() {
                let in_dir = if k == 0 { step_dir[0] } else { step_dir[k - 1] };
                let out_dir = step_dir.get(k).copied().unwrap_or(in_dir);
                let glyph = corner_glyph(opposite(in_dir), out_dir);

                // Comet brightness: how far the head has travelled past this cell.
                let behind = (head_pos - k as f32).rem_euclid(period);
                let head = (1.0 - behind / PULSE_HEAD).max(0.0);
                let tail = (1.0 - behind / PULSE_TAIL).max(0.0) * TAIL_GAIN;
                let lead = (1.0 - (period - behind) / PULSE_LEAD).max(0.0) * LEAD_GAIN;
                let boost = head.max(tail).max(lead);

                // Radial vignette.
                let nx = px as f32 * inv_w - 0.5;
                let ny = py as f32 * inv_h - 0.5;
                let vignette = (1.0 - (nx * nx + ny * ny) * 4.0 * EDGE_FADE).clamp(0.0, 1.0);

                // Calm the central band where the logo and text live.
                let dist = (py as f32 - center_y).abs();
                let center_dim = if dist >= CENTER_HALF_BAND {
                    1.0
                } else {
                    CENTER_MIN_DIM + (1.0 - CENTER_MIN_DIM) * (dist / CENTER_HALF_BAND)
                };

                let alpha = ((BASE_ALPHA + boost * PULSE_GAIN) * fade * vignette * center_dim)
                    .clamp(0.0, ALPHA_MAX);
                if alpha <= 0.0 {
                    continue;
                }

                let fg = Color::Rgb(
                    lerp_u8(bg.0, color.0, alpha),
                    lerp_u8(bg.1, color.1, alpha),
                    lerp_u8(bg.2, color.2, alpha),
                );
                let sx = area.x + px as u16;
                let sy = area.y + py as u16;
                if let Some(cell) = buf.cell_mut((sx, sy)) {
                    cell.set_char(glyph).set_style(Style::new().fg(fg));
                }
            }
        }
    }

    fn render_logo(
        &self,
        area: Rect,
        buf: &mut Buffer,
        t: f32,
        fade: f32,
        top_y: u16,
        accent: Color,
    ) {
        let theme = theme::current();
        let bg = theme.background;
        let (ac_r, ac_g, ac_b) = extract_rgb(accent, (100, 140, 255));
        let (bg_r, bg_g, bg_b) = extract_rgb(bg, (15, 15, 25));

        let logo_x = area.x + (area.width.saturating_sub(LOGO.len() as u16)) / 2;
        let alpha = 0.85 * ease_out_cubic(((t - LOGO_DELAY) / LOGO_RAMP).clamp(0.0, 1.0)) * fade;
        let style = Style::new()
            .fg(Color::Rgb(
                lerp_u8(bg_r, ac_r, alpha),
                lerp_u8(bg_g, ac_g, alpha),
                lerp_u8(bg_b, ac_b.saturating_add(15), alpha),
            ))
            .bg(bg)
            .add_modifier(Modifier::BOLD);

        for (col, ch) in LOGO.chars().enumerate() {
            let x = logo_x + col as u16;
            if x >= area.x + area.width || top_y >= area.y + area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, top_y)) {
                cell.set_char(ch).set_style(style);
            }
        }
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer, fade: f32, help_y: u16, accent: Color) {
        if help_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let bg = theme.background;
        let ac = extract_rgb(accent, (100, 140, 255));
        let fg = extract_rgb(theme.foreground, (200, 200, 200));
        let bg_rgb = extract_rgb(bg, (15, 15, 25));

        let total_width: u16 = HELP_SEGMENTS.iter().map(|(s, _)| s.len() as u16).sum();
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: Vec<_> = HELP_SEGMENTS
            .iter()
            .map(|&(text, highlighted)| {
                let (target, alpha) = if highlighted { (ac, 0.75) } else { (fg, 0.5) };
                (text, faded_style(bg_rgb, target, alpha * fade, bg))
            })
            .collect();

        render_segments(area, buf, help_y, x_start, &segments);
    }

    fn render_tip(&self, area: Rect, buf: &mut Buffer, fade: f32, tip_y: u16, accent: Color) {
        if tip_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let bg = theme.background;
        let tip_rgb = extract_rgb(
            theme.todo_in_progress.fg.unwrap_or(Color::Yellow),
            (249, 226, 175),
        );
        let ac = extract_rgb(accent, (100, 140, 255));
        let fg = extract_rgb(theme.foreground, (200, 200, 200));
        let bg_rgb = extract_rgb(bg, (15, 15, 25));

        let (label, desc) = TIPS[self.tip_idx];
        let total_width = (5 + label.len() + 1 + desc.len()) as u16;
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: &[(&str, Style)] = &[
            (
                "tip: ",
                faded_style(bg_rgb, tip_rgb, 0.75 * fade, bg).add_modifier(Modifier::BOLD),
            ),
            (label, faded_style(bg_rgb, ac, 0.75 * fade, bg)),
            (" ", Style::default()),
            (desc, faded_style(bg_rgb, fg, 0.5 * fade, bg)),
        ];

        render_segments(area, buf, tip_y, x_start, segments);
    }
}

fn render_version(area: Rect, buf: &mut Buffer, fade: f32, y: u16, new_version: Option<&str>) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let bg = theme.background;
    let text = match new_version {
        Some(v) => format!("v{} run craft update to get v{}", update::CURRENT, v),
        None => format!("v{}", update::CURRENT),
    };
    let style = faded_style(
        extract_rgb(bg, (15, 15, 25)),
        extract_rgb(theme.foreground, (200, 200, 200)),
        0.4 * fade,
        bg,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16 + 1);
    render_segments(area, buf, y, x_start, &[(&text, style)]);
}

fn render_centered_faded(
    area: Rect,
    buf: &mut Buffer,
    fade: f32,
    intensity: f32,
    y: u16,
    text: &str,
) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let bg = theme.background;
    let style = faded_style(
        extract_rgb(bg, (15, 15, 25)),
        extract_rgb(theme.foreground, (200, 200, 200)),
        intensity * fade,
        bg,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16) / 2;
    render_segments(area, buf, y, x_start, &[(text, style)]);
}

fn extract_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => fallback,
    }
}

fn faded_style(bg: (u8, u8, u8), fg: (u8, u8, u8), alpha: f32, bg_color: Color) -> Style {
    Style::new()
        .fg(Color::Rgb(
            lerp_u8(bg.0, fg.0, alpha),
            lerp_u8(bg.1, fg.1, alpha),
            lerp_u8(bg.2, fg.2, alpha),
        ))
        .bg(bg_color)
}

fn render_segments(area: Rect, buf: &mut Buffer, y: u16, x_start: u16, segments: &[(&str, Style)]) {
    let x_end = area.x + area.width;
    let mut x = x_start;
    for &(text, style) in segments {
        for ch in text.chars() {
            if x >= x_end {
                return;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch).set_style(style);
            }
            x += 1;
        }
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn transition_at(from: (u8, u8, u8), to: (u8, u8, u8), offset: Duration) -> (u8, u8, u8) {
        let mut ct = ColorTransition::new(Color::Rgb(from.0, from.1, from.2));
        ct.set(Color::Rgb(to.0, to.1, to.2));
        ct.resolve_rgb(ct.start + offset)
    }

    #[test]
    fn interpolation_over_time() {
        let start = transition_at((0, 0, 0), (200, 200, 200), Duration::ZERO);
        assert_eq!(start, (0, 0, 0));

        let mid = transition_at((0, 0, 0), (200, 200, 200), Duration::from_millis(200));
        assert!(
            mid.0 > 0 && mid.0 < 200,
            "expected interpolated, got {}",
            mid.0
        );

        let done = transition_at((0, 0, 0), (255, 255, 255), Duration::from_millis(500));
        assert_eq!(done, (255, 255, 255));
    }

    #[test]
    fn chained_set_restarts_toward_new_target() {
        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(200, 100, 50));
        ct.set(Color::Rgb(10, 20, 30));

        let done = ct.resolve_rgb(ct.start + Duration::from_secs(1));
        assert_eq!(done, (10, 20, 30));
    }

    #[test]
    fn is_animating_lifecycle() {
        let ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        assert!(!ct.is_animating(), "settled on construction");

        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(255, 0, 0));
        assert!(ct.is_animating(), "animating after set");
    }

    #[test]
    fn non_rgb_color_uses_fallback() {
        let ct = ColorTransition::new(Color::Blue);
        assert_eq!(
            ct.resolve_rgb(ct.start + Duration::from_secs(1)),
            (100, 140, 255)
        );
    }
}
