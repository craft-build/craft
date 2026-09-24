//! Startup splash: the "craft" wordmark over an animated field of
//! Manhattan circuit traces, ported from the reference `craft-ui`
//! `splash.rs`. Rendering is pure — time and the RNG seed are parameters —
//! so `TestBackend` tests reproduce frames exactly.
//!
//! Dropped from the reference (features this repo does not have yet):
//! update-hint version notices, keybinding/tip rows tied to the file
//! picker, /tasks, /btw and /cd, and `ColorTransition` (our theme is a
//! fixed set of constants, so there is nothing to crossfade).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::theme;

const LOGO: &str = "craft";
const TAGLINE: &str = "Craft can edit files, run commands, and search the web on your behalf.";

/// Total seconds the splash plays before the main screen takes over.
pub const SPLASH_SECS: f32 = 2.4;

/// Seconds for the initial fade-in animation (ease-out cubic).
const FADE_DURATION: f32 = 1.6;
/// Seconds to wait before the logo starts appearing.
const LOGO_DELAY: f32 = 0.2;
/// Seconds over which the logo fades from dim to full brightness.
const LOGO_RAMP: f32 = 0.8;
/// Period of the logo's resting brightness breathing pulse, once ramped in.
const GLOW_PERIOD_SECS: f32 = 2.4;
/// How far the breathing pulse dips brightness below full (0 = none).
const GLOW_DEPTH: f32 = 0.12;

// ── Brand colors ─────────────────────────────────────────────────────────────

/// Brand blue — the gradient's near stop (`--blue-500`).
const BRAND_BLUE: (u8, u8, u8) = (79, 141, 255);
/// Brand violet — the gradient's mid stop, and the primary wiring signal.
const WIRE_VIOLET: (u8, u8, u8) = (148, 87, 242);
/// Brand magenta — the gradient's far stop, and the secondary signal.
const WIRE_MAGENTA: (u8, u8, u8) = (225, 63, 234);
/// Where the blue→violet→magenta gradient crosses its middle stop.
const GRADIENT_MID: f32 = 0.55;

// ── Ambient glow ─────────────────────────────────────────────────────────────

/// Focal point of the glow, as a fraction of the splash area (x, y).
const GLOW_CENTER: (f32, f32) = (0.5, 0.34);
/// Glow radius as a fraction of area width; the vertical radius is derived
/// from this, corrected for terminal cells being roughly twice as tall as
/// they are wide, so the glow reads as a rounded blob rather than a diamond.
const GLOW_RADIUS_X_FRAC: f32 = 0.45;
const CHAR_ASPECT: f32 = 2.0;
/// Peak tint strength at the focal point.
const GLOW_MAX_ALPHA: f32 = 0.30;

// ── Wiring field ─────────────────────────────────────────────────────────────

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
/// Slight lean toward Down/Right at corners.
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
/// Dim the central rows so the logo and tagline sit on a calm backdrop.
const CENTER_HALF_BAND: f32 = 4.0;
const CENTER_MIN_DIM: f32 = 0.45;

/// Direction unit vectors, indexed Right, Down, Left, Up.
const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

/// Below this size the splash draws nothing (reference threshold).
pub const MIN_WIDTH: u16 = 20;
pub const MIN_HEIGHT: u16 = 5;

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

/// Minimal deterministic xorshift32 — keeps trace layout stable across
/// frames (only the pulse moves) without pulling in an rng dependency.
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

/// One splash instance: the trace layout is fixed by `seed`, the field
/// clock starts at `field_offset` so consecutive runs never sync up.
pub struct Splash {
    seed: u32,
    field_offset: f32,
}

impl Splash {
    /// Runtime constructor: seed and offset from the wall clock.
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self::with_seed(
            (nanos & 0xffff_ffff) as u32,
            ((nanos >> 32) % 10_000) as f32,
        )
    }

    /// Deterministic constructor (tests).
    pub fn with_seed(seed: u32, field_offset: f32) -> Self {
        Self { seed, field_offset }
    }

    /// Paint one frame at elapsed time `t` seconds. Pure: same seed, same
    /// `t`, same buffer.
    pub fn render(&self, area: Rect, buf: &mut Buffer, t: f32) {
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return;
        }

        let fade = if t >= FADE_DURATION {
            1.0
        } else {
            ease_out_cubic(t / FADE_DURATION)
        };

        let block_height = 8;
        let top_y = area.y + area.height.saturating_sub(block_height) / 2;
        let tag_y = top_y + 1;

        render_glow(area, buf, fade);
        self.render_wiring(area, buf, t + self.field_offset, fade);
        self.render_logo(area, buf, t, fade, top_y);
        render_centered_faded(area, buf, fade, 0.75, tag_y, TAGLINE);
        render_version(area, buf, fade, area.y);
    }

    /// The wiring is a gradient of intensities between the background and
    /// the signal colors, so it needs both as numbers. On a palette theme
    /// there is nothing to interpolate and the wiring simply does not draw.
    fn render_wiring(&self, area: Rect, buf: &mut Buffer, clock: f32, fade: f32) {
        let Some((bg_r, bg_g, bg_b)) = rgb(theme::BG_APP) else {
            return;
        };
        let bg = (bg_r, bg_g, bg_b);
        let palette = [
            WIRE_VIOLET,
            WIRE_MAGENTA,
            rgb_or(theme::ACCENT, WIRE_VIOLET),
        ];

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
            // Seed from the per-instance layout seed only (not the clock),
            // so wires stay put frame to frame and just the pulse slides
            // along them.
            let mut rng = Rng::new(self.seed ^ (ti as u32).wrapping_mul(0x9E37_79B9));
            let color = palette[ti % palette.len()];
            let phase = rng.next_f32();

            // Start anywhere in the field, heading in any direction, so
            // traces spread evenly instead of piling up along the edges.
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
            let period = len as f32 + PULSE_GAP;
            // Pulse head position along the path, wrapping through the gap.
            let head_pos = (clock * PULSE_SPEED + phase * period).rem_euclid(period);

            for (k, &(px, py)) in pts.iter().enumerate() {
                let in_dir = if k == 0 { step_dir[0] } else { step_dir[k - 1] };
                let out_dir = step_dir.get(k).copied().unwrap_or(in_dir);
                let glyph = corner_glyph(opposite(in_dir), out_dir);

                // Comet brightness: how far the head has travelled past this
                // cell.
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
                if let Some(cell) = buf.cell_mut((area.x + px as u16, area.y + py as u16)) {
                    cell.set_char(glyph).set_style(Style::new().fg(fg));
                }
            }
        }
    }

    /// Renders the wordmark with the fixed brand gradient (blue → violet →
    /// magenta), one stop per character, echoing `--grad-brand`.
    fn render_logo(&self, area: Rect, buf: &mut Buffer, t: f32, fade: f32, top_y: u16) {
        let bg = theme::BG_APP;

        let logo_x = area.x + (area.width.saturating_sub(LOGO.len() as u16)) / 2;
        let ramp = ease_out_cubic(((t - LOGO_DELAY) / LOGO_RAMP).clamp(0.0, 1.0));
        // Gentle breathing glow, echoing the brand's pulsing gradient
        // without ever going fully dim; negligible while still ramping in.
        let glow =
            1.0 - GLOW_DEPTH * (0.5 - 0.5 * (t * std::f32::consts::TAU / GLOW_PERIOD_SECS).cos());
        let alpha = 0.95 * ramp * glow * fade;

        let last = LOGO.chars().count().saturating_sub(1).max(1) as f32;
        for (col, ch) in LOGO.chars().enumerate() {
            let x = logo_x + col as u16;
            if x >= area.x + area.width || top_y >= area.y + area.height {
                continue;
            }
            let stop = col as f32 / last;
            let (r, g, b) = gradient_stop(stop);
            let style = faded_style(Color::Rgb(r, g, b), bg, alpha).add_modifier(Modifier::BOLD);
            if let Some(cell) = buf.cell_mut((x, top_y)) {
                cell.set_char(ch).set_style(style);
            }
        }
    }
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn rgb_or(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    rgb(color).unwrap_or(fallback)
}

fn render_version(area: Rect, buf: &mut Buffer, fade: f32, y: u16) {
    if y >= area.y + area.height {
        return;
    }
    let text = format!("v{}", env!("CARGO_PKG_VERSION"));
    let style = faded_style(theme::TEXT_SECONDARY, theme::BG_APP, 0.4 * fade);
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
    let style = faded_style(theme::TEXT_SECONDARY, theme::BG_APP, intensity * fade);
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16) / 2;
    render_segments(area, buf, y, x_start, &[(text, style)]);
}

/// Fades `fg` in from `bg` at `alpha`, leaving the cell's background alone
/// so the ambient glow painted by [`render_glow`] shows through underneath.
fn faded_style(fg: Color, bg: Color, alpha: f32) -> Style {
    match (fg, bg) {
        (Color::Rgb(fr, fg_, fb), Color::Rgb(br, bg_, bb)) => Style::new().fg(Color::Rgb(
            lerp_u8(br, fr, alpha),
            lerp_u8(bg_, fg_, alpha),
            lerp_u8(bb, fb, alpha),
        )),
        _ => Style::new().fg(fg),
    }
}

/// Blue → violet → magenta stop at `t` in `[0, 1]`.
fn gradient_stop(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    if t < GRADIENT_MID {
        lerp_rgb(BRAND_BLUE, WIRE_VIOLET, t / GRADIENT_MID)
    } else {
        lerp_rgb(
            WIRE_VIOLET,
            WIRE_MAGENTA,
            (t - GRADIENT_MID) / (1.0 - GRADIENT_MID),
        )
    }
}

fn lerp_rgb(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    (
        lerp_u8(from.0, to.0, t),
        lerp_u8(from.1, to.1, t),
        lerp_u8(from.2, to.2, t),
    )
}

fn lerp_u8(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * t.clamp(0.0, 1.0)).round() as u8
}

/// Paints a soft radial wash across the whole splash area before anything
/// else draws. Only touches cell backgrounds — chars and fg are left alone.
fn render_glow(area: Rect, buf: &mut Buffer, fade: f32) {
    let Some(bg) = rgb(theme::BG_APP) else {
        return;
    };
    let w = area.width as f32;
    let h = area.height as f32;
    if w < 1.0 || h < 1.0 {
        return;
    }
    let fx = w * GLOW_CENTER.0;
    let fy = h * GLOW_CENTER.1;
    let rx = (w * GLOW_RADIUS_X_FRAC).max(1.0);
    let ry = (rx / CHAR_ASPECT).max(1.0);

    for row in 0..area.height {
        let ny = (row as f32 - fy) / ry;
        for col in 0..area.width {
            let nx = (col as f32 - fx) / rx;
            let d2 = nx * nx + ny * ny;
            let alpha = (GLOW_MAX_ALPHA * (1.0 - d2)).clamp(0.0, GLOW_MAX_ALPHA) * fade;
            let (r, g, b) = if alpha <= 0.0 {
                bg
            } else {
                lerp_rgb(bg, WIRE_VIOLET, alpha)
            };
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                cell.set_bg(Color::Rgb(r, g, b));
            }
        }
    }
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

    fn rendered(seed: u32, t: f32) -> Buffer {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        Splash::with_seed(seed, 0.0).render(area, &mut buf, t);
        buf
    }

    fn buffer_text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf[(c, r)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn same_seed_and_time_render_identically() {
        let a = rendered(42, 1.0);
        let b = rendered(42, 1.0);
        assert_eq!(buffer_text(&a), buffer_text(&b));
        for (ca, cb) in a.content.iter().zip(b.content.iter()) {
            assert_eq!(ca.style(), cb.style());
        }
    }

    #[test]
    fn time_moves_the_logo_brightness() {
        let early = rendered(7, LOGO_DELAY + 0.05);
        let late = rendered(7, LOGO_DELAY + LOGO_RAMP);
        let brightness = |buf: &Buffer| {
            buf.content
                .iter()
                .filter(|c| c.symbol() == "c" || c.symbol() == "r" || c.symbol() == "a")
                .filter_map(|c| match c.style().fg {
                    Some(Color::Rgb(r, g, b)) => Some(r as u32 + g as u32 + b as u32),
                    _ => None,
                })
                .sum::<u32>()
        };
        assert!(
            brightness(&late) > brightness(&early),
            "the logo must brighten as it ramps in"
        );
    }

    #[test]
    fn logo_and_tagline_appear_once_faded_in() {
        let text = buffer_text(&rendered(1, SPLASH_SECS));
        let row = text
            .split('\n')
            .find(|r| r.contains("craft"))
            .expect("logo row");
        assert!(row.contains("craft"));
        assert!(text.contains("on your behalf"));
    }

    #[test]
    fn at_time_zero_nothing_is_visible_yet() {
        // t=0: fade=0, so every painted fg is exactly the background color.
        let buf = rendered(3, 0.0);
        assert!(
            buf.content.iter().all(|c| match c.style().fg {
                Some(Color::Rgb(r, g, b)) => (r, g, b) == rgb_or(theme::BG_APP, (0, 0, 0)),
                _ => true,
            }),
            "the entry fade starts from a blank screen"
        );
    }

    #[test]
    fn tiny_areas_draw_nothing() {
        let area = Rect::new(0, 0, MIN_WIDTH - 1, 24);
        let mut buf = Buffer::empty(area);
        Splash::with_seed(1, 0.0).render(area, &mut buf, SPLASH_SECS);
        assert!(buf.content.iter().all(|c| c.symbol() == " "));
    }

    #[test]
    fn different_seeds_lay_out_different_traces() {
        let a = buffer_text(&rendered(11, 1.0));
        let b = buffer_text(&rendered(12, 1.0));
        assert_ne!(a, b);
    }
}
