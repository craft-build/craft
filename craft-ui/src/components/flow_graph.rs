//! Mermaid-rendered workflow graph for Flow mode.
//!
//! Given a [`FlowSnapshot`] the graph builds a Mermaid `flowchart TD` source
//! describing the workstream's pipeline (Scout -> TPM -> Plan -> chunks ->
//! Integrator -> Verifier), highlights the active stage/chunk, renders it to
//! PNG via `mermaid-rs-renderer`, and hands the bytes to [`ImagePicker`] for
//! terminal display. The rendered image is cached by source hash + target
//! dimensions so unchanged snapshots don't re-rasterize on every frame.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use craft_agent::{ThreadStatus, TurnType};
use ratatui::style::Color;

use crate::components::flow_panel::{FlowSnapshot, FlowSnapshotChunk};
use crate::image_render::{ImagePicker, ImageRenderState};
use crate::theme;

const MAX_LABEL_WIDTH_CHARS: usize = 16;
const NODE_SPACING: f32 = 36.0;
const RANK_SPACING: f32 = 40.0;
const NODE_PADDING_X: f32 = 18.0;
const NODE_PADDING_Y: f32 = 10.0;

/// Hex color strings coordinated with the active craft terminal theme, so the
/// rendered diagram blends into the TUI instead of using mermaid's bright
/// default white background.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GraphColors {
    background: String,
    text: String,
    line: String,
    node_default: String,
    node_active: String,
    node_done: String,
    node_blocked: String,
}

impl GraphColors {
    fn from_theme() -> Self {
        let t = theme::current();
        let bg = hex(t.background).unwrap_or_else(|| "#1e1e2e".into());
        let fg = hex(t.foreground).unwrap_or_else(|| "#cdd6f4".into());
        // Dim the background slightly for node bodies so they stand out from
        // the canvas without clashing.
        let node_default = lighten(&bg, 0.12);
        Self {
            background: bg.clone(),
            text: fg.clone(),
            line: dim(&fg, 0.5),
            node_default,
            node_active: hex_style_fg(t.active).unwrap_or_else(|| "#f9e2af".into()),
            node_done: hex(t.tool_success.fg.unwrap_or(t.foreground))
                .unwrap_or_else(|| "#a6e3a1".into()),
            node_blocked: hex(t.tool_error.fg.unwrap_or(t.foreground))
                .unwrap_or_else(|| "#f38ba8".into()),
        }
    }
}

/// Convert a ratatui `Color` to a `#rrggbb` hex string. Returns `None` for
/// non-RGB colors (Reset, Indexed, named defaults) so callers can fall back.
fn hex(c: Color) -> Option<String> {
    match c {
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

/// Extract the foreground color of a `Style` as hex.
fn hex_style_fg(s: ratatui::style::Style) -> Option<String> {
    s.fg.and_then(hex)
}

/// Parse a `#rrggbb` string to (r, g, b) u8.
fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Convert a `#rrggbb` string to a tiny-skia `Color` (opaque). Falls back to a
/// dark background if the string is malformed.
fn color_from_hex(s: &str) -> tiny_skia::Color {
    parse_hex(s)
        .map(|(r, g, b)| tiny_skia::Color::from_rgba8(r, g, b, 0xff))
        .unwrap_or_else(|| tiny_skia::Color::from_rgba8(0x1e, 0x1e, 0x2e, 0xff))
}

/// Blend a `#rrggbb` color toward white by `factor` (0.0 = unchanged, 1.0 =
/// white). Used to derive a node body color that stands out from the canvas.
fn lighten(s: &str, factor: f32) -> String {
    let Some((r, g, b)) = parse_hex(s) else {
        return s.to_string();
    };
    let lerp = |c: u8| ((c as f32) + (255.0 - c as f32) * factor).round() as u8;
    let (lr, lg, lb) = (lerp(r), lerp(g), lerp(b));
    format!("#{lr:02x}{lg:02x}{lb:02x}")
}

/// Blend a `#rrggbb` color toward the canvas background by `factor`, producing
/// a dimmer shade for edges and secondary lines.
fn dim(s: &str, factor: f32) -> String {
    let t = theme::current();
    let bg = hex(t.background).unwrap_or_else(|| "#1e1e2e".into());
    let Some((r, g, b)) = parse_hex(s) else {
        return s.to_string();
    };
    let Some((br, bgr, bb)) = parse_hex(&bg) else {
        return s.to_string();
    };
    let mix = |c: u8, bc: u8| ((c as f32) * (1.0 - factor) + (bc as f32) * factor).round() as u8;
    let (mr, mg, mb) = (mix(r, br), mix(g, bgr), mix(b, bb));
    format!("#{mr:02x}{mg:02x}{mb:02x}")
}

/// A cached rendered workflow graph. Reuse across frames; call [`Self::render`]
/// whenever the snapshot or target dimensions change.
pub(crate) struct FlowGraph {
    cache_key: Option<u64>,
    image: Option<ImageRenderState>,
}

impl FlowGraph {
    pub(crate) fn new() -> Self {
        Self {
            cache_key: None,
            image: None,
        }
    }

    /// Render (or return cached) graph image for `snapshot` fit into the given
    /// cell box. Returns `None` when the diagram fails to render or the
    /// terminal has no usable image protocol. The picker is reused from the
    /// messages panel so the protocol detection and font metrics match.
    pub(crate) fn render(
        &mut self,
        snapshot: &FlowSnapshot,
        picker: &ImagePicker,
        width: u16,
        height: u16,
    ) -> Option<&ImageRenderState> {
        if width < 4 || height < 2 {
            return None;
        }
        let colors = GraphColors::from_theme();
        let source = build_mermaid(snapshot, &colors);
        let key = cache_key(&source, width, height, &colors);
        if self.cache_key == Some(key) && self.image.is_some() {
            return self.image.as_ref();
        }
        let font = picker.font_size();
        let target_px = (
            (width as u32).saturating_mul(font.width.max(1) as u32),
            (height as u32).saturating_mul(font.height.max(1) as u32),
        );
        let png = render_png(&source, target_px, &colors);
        if let Err(e) = &png {
            tracing::warn!(error = %e, "flow graph render failed");
        }
        let state = match png {
            Ok(bytes) => picker.render_png(&bytes, width, height),
            Err(_) => None,
        };
        self.cache_key = Some(key);
        self.image = state;
        self.image.as_ref()
    }

    #[allow(dead_code)]
    pub(crate) fn reset(&mut self) {
        self.cache_key = None;
        self.image = None;
    }
}

fn cache_key(source: &str, width: u16, height: u16, colors: &GraphColors) -> u64 {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    width.hash(&mut h);
    height.hash(&mut h);
    colors.hash(&mut h);
    h.finish()
}

/// Render the mermaid diagram to PNG bytes, using `colors` for the mermaid
/// theme (background/text/edges) and scaled to fit `target_px`.
fn render_png(
    source: &str,
    target_px: (u32, u32),
    colors: &GraphColors,
) -> Result<Vec<u8>, String> {
    let theme = mermaid_theme(colors);
    let layout = mermaid_rs_renderer::LayoutConfig {
        max_label_width_chars: MAX_LABEL_WIDTH_CHARS,
        node_spacing: NODE_SPACING,
        rank_spacing: RANK_SPACING,
        node_padding_x: NODE_PADDING_X,
        node_padding_y: NODE_PADDING_Y,
        ..Default::default()
    };
    let options = mermaid_rs_renderer::RenderOptions { theme, layout };
    let svg = mermaid_rs_renderer::render_with_options(source, options)
        .map_err(|e| format!("svg render: {e}"))?;
    rasterize_svg(&svg, target_px, colors)
}

/// Build a mermaid `Theme` whose background, text, and edge colors match the
/// active craft terminal theme so the diagram blends into the TUI.
fn mermaid_theme(colors: &GraphColors) -> mermaid_rs_renderer::Theme {
    let mut t = mermaid_rs_renderer::Theme::mermaid_default();
    t.background = colors.background.clone();
    t.primary_color = colors.node_default.clone();
    t.primary_text_color = colors.text.clone();
    t.primary_border_color = colors.line.clone();
    t.line_color = colors.line.clone();
    t.secondary_color = colors.node_default.clone();
    t.tertiary_color = colors.node_default.clone();
    t.edge_label_background = colors.background.clone();
    t.cluster_background = colors.background.clone();
    t.cluster_border = colors.line.clone();
    t.text_color = colors.text.clone();
    t.sequence_actor_fill = colors.node_default.clone();
    t.sequence_actor_border = colors.line.clone();
    t.sequence_actor_line = colors.line.clone();
    t.sequence_note_fill = colors.node_default.clone();
    t.sequence_note_border = colors.line.clone();
    t.sequence_activation_fill = colors.node_default.clone();
    t.sequence_activation_border = colors.line.clone();
    t
}

/// Parse `svg` with usvg and rasterize it with resvg to fit the target
/// pixel box while preserving the diagram's aspect ratio. The scaled diagram
/// is centered and the surrounding letterbox is filled with the theme
/// background so the panel looks clean when the graph's shape doesn't match
/// the cell box.
fn rasterize_svg(
    svg: &str,
    target_px: (u32, u32),
    colors: &GraphColors,
) -> Result<Vec<u8>, String> {
    let (tw, th) = target_px;
    if tw == 0 || th == 0 {
        return Err("zero target size".into());
    }
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| format!("usvg parse: {e}"))?;
    let nat = tree.size().to_int_size();
    let (nw, nh) = (nat.width() as f32, nat.height() as f32);
    if nw < 1.0 || nh < 1.0 {
        return Err("svg has no intrinsic size".into());
    }
    let scale = ((tw as f32) / nw).min((th as f32) / nh);
    let scaled_w = nw * scale;
    let scaled_h = nh * scale;
    let tx = ((tw as f32) - scaled_w) / 2.0;
    let ty = ((th as f32) - scaled_h) / 2.0;
    let mut pixmap = tiny_skia::Pixmap::new(tw, th).ok_or_else(|| "pixmap alloc".to_string())?;
    pixmap.fill(color_from_hex(&colors.background));
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|e| format!("png encode: {e}"))
}

/// Build the Mermaid `flowchart TD` source for a snapshot.
///
/// Layout: Scout -> TPM -> Plan, then Plan fans into every chunk (drawn as a
/// parallel band when `parallel_chunks > 1` or multiple chunks are currently
/// running, otherwise a linear chain), then chunks fan into Integrator ->
/// Verifier. Active stage/chunk nodes get the active fill; done/blocked get
/// their semantic colors.
fn build_mermaid(snapshot: &FlowSnapshot, colors: &GraphColors) -> String {
    let mut out = String::new();
    out.push_str("flowchart TD\n");

    let current_stage = snapshot.stage;
    for stage in [TurnType::Scout, TurnType::Tpm, TurnType::Plan] {
        node_rounded(
            &mut out,
            stage_id(stage),
            stage.as_str(),
            stage_fill(stage, current_stage, colors),
            colors,
        );
    }
    edge(&mut out, stage_id(TurnType::Scout), stage_id(TurnType::Tpm));
    edge(&mut out, stage_id(TurnType::Tpm), stage_id(TurnType::Plan));

    if snapshot.chunks.is_empty() {
        return out;
    }

    let chunks: Vec<(&String, &FlowSnapshotChunk)> = {
        let mut v: Vec<_> = snapshot.chunks.iter().collect();
        v.sort_by_key(|(_, c)| c.order);
        v
    };
    for (id, chunk) in &chunks {
        let fill = chunk_fill(chunk, colors);
        let label = chunk_label(chunk, id);
        node_rounded(&mut out, &sanitize_id(id), &label, fill, colors);
    }

    // Render the plan DAG: each chunk's declared `depends_on` become edges.
    // Chunks with no declared deps chain off Plan in plan order (the plan's
    // implicit topological sequence); the last chunk(s) with no dependents
    // chain into Integrator.
    let has_any_dep = chunks.iter().any(|(_, c)| !c.depends_on.is_empty());
    if has_any_dep {
        for (id, chunk) in &chunks {
            let sid = sanitize_id(id);
            if chunk.depends_on.is_empty() {
                edge(&mut out, stage_id(TurnType::Plan), &sid);
            } else {
                for dep in &chunk.depends_on {
                    edge(&mut out, &sanitize_id(dep), &sid);
                }
            }
        }
        // Chunks that nothing depends on chain into Integrator.
        let all_deps: std::collections::HashSet<&str> = chunks
            .iter()
            .flat_map(|(_, c)| c.depends_on.iter().map(String::as_str))
            .collect();
        for (id, _) in &chunks {
            if !all_deps.contains(id.as_str()) {
                edge(&mut out, &sanitize_id(id), stage_id(TurnType::Integrator));
            }
        }
    } else {
        // No explicit deps: chain in plan order.
        let mut prev: &str = stage_id(TurnType::Plan);
        for (id, _) in &chunks {
            let sid = sanitize_id(id);
            edge(&mut out, prev, &sid);
            prev = sid.leak();
        }
        edge(&mut out, prev, stage_id(TurnType::Integrator));
    }

    for stage in [TurnType::Integrator, TurnType::Verifier] {
        node_rounded(
            &mut out,
            stage_id(stage),
            stage.as_str(),
            stage_fill(stage, current_stage, colors),
            colors,
        );
    }
    edge(
        &mut out,
        stage_id(TurnType::Integrator),
        stage_id(TurnType::Verifier),
    );
    out
}

fn stage_id(stage: TurnType) -> &'static str {
    match stage {
        TurnType::General => "stage_general",
        TurnType::Scout => "stage_scout",
        TurnType::Tpm => "stage_tpm",
        TurnType::Plan => "stage_plan",
        TurnType::Req => "stage_req",
        TurnType::Execute => "stage_execute",
        TurnType::Review => "stage_review",
        TurnType::Qa => "stage_qa",
        TurnType::Report => "stage_report",
        TurnType::Integrator => "stage_integrator",
        TurnType::Verifier => "stage_verifier",
    }
}

fn stage_fill(stage: TurnType, current: Option<TurnType>, colors: &GraphColors) -> &str {
    match current {
        Some(c) if c == stage => &colors.node_active,
        Some(c) if stage_is_past(stage, c) => &colors.node_done,
        _ => &colors.node_default,
    }
}

fn stage_is_past(stage: TurnType, current: TurnType) -> bool {
    stage_order(stage) < stage_order(current)
}

fn stage_order(stage: TurnType) -> u8 {
    match stage {
        TurnType::General => 0,
        TurnType::Scout => 1,
        TurnType::Tpm => 2,
        TurnType::Plan => 3,
        TurnType::Req => 4,
        TurnType::Execute => 5,
        TurnType::Review => 6,
        TurnType::Qa => 7,
        TurnType::Report => 8,
        TurnType::Integrator => 9,
        TurnType::Verifier => 10,
    }
}

fn chunk_fill<'a>(chunk: &FlowSnapshotChunk, colors: &'a GraphColors) -> &'a str {
    match chunk.status {
        ThreadStatus::Done => &colors.node_done,
        ThreadStatus::Blocked => &colors.node_blocked,
        ThreadStatus::Running => &colors.node_active,
        _ => &colors.node_default,
    }
}

fn chunk_label(chunk: &FlowSnapshotChunk, id: &str) -> String {
    let title = if chunk.title.is_empty() {
        id
    } else {
        &chunk.title
    };
    let mut label = title.replace('"', "'");
    if chunk.status == ThreadStatus::Running
        && let Some(stage) = chunk.stage
    {
        label.push_str(&format!("\\n{}", stage.as_str()));
    }
    label
}

fn node_rounded(out: &mut String, id: &str, label: &str, fill: &str, colors: &GraphColors) {
    out.push_str(&format!("    {id}(\"{label}\")\n"));
    let stroke = &colors.line;
    let text = &colors.text;
    out.push_str(&format!(
        "    style {id} fill:{fill},stroke:{stroke},color:{text}\n"
    ));
}

fn edge(out: &mut String, from: &str, to: &str) {
    out.push_str(&format!("    {from} --> {to}\n"));
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn chunk(title: &str, status: ThreadStatus) -> FlowSnapshotChunk {
        FlowSnapshotChunk {
            title: title.into(),
            status,
            stage: None,
            ..Default::default()
        }
    }

    fn snapshot(
        stage: Option<TurnType>,
        chunks: BTreeMap<String, FlowSnapshotChunk>,
    ) -> FlowSnapshot {
        FlowSnapshot {
            workstream_id: "ws".into(),
            stage,
            chunks,
            ..Default::default()
        }
    }

    fn test_colors() -> GraphColors {
        GraphColors {
            background: "#1e1e2e".into(),
            text: "#cdd6f4".into(),
            line: "#6c7086".into(),
            node_default: "#313244".into(),
            node_active: "#f9e2af".into(),
            node_done: "#a6e3a1".into(),
            node_blocked: "#f38ba8".into(),
        }
    }

    #[test]
    fn pre_plan_only_before_chunks() {
        let s = snapshot(Some(TurnType::Scout), BTreeMap::new());
        let m = build_mermaid(&s, &test_colors());
        assert!(m.contains("stage_scout"));
        assert!(m.contains("stage_tpm"));
        assert!(m.contains("stage_plan"));
        assert!(!m.contains("stage_integrator"));
    }

    #[test]
    fn linear_chain_when_no_deps() {
        // Chunks with no depends_on chain in plan order.
        let mut chunks = BTreeMap::new();
        chunks.insert("c1".into(), chunk("First", ThreadStatus::Done));
        chunks.insert("c2".into(), chunk("Second", ThreadStatus::Running));
        let s = snapshot(Some(TurnType::Execute), chunks);
        let m = build_mermaid(&s, &test_colors());
        assert!(m.contains("stage_plan --> c1"));
        assert!(m.contains("c1 --> c2"));
        assert!(m.contains("c2 --> stage_integrator"));
    }

    #[test]
    fn dag_renders_dependency_edges() {
        // A -> B, A -> C, [B+C] -> D
        let mut chunks = BTreeMap::new();
        let mut a = chunk("A", ThreadStatus::Done);
        a.order = 0;
        let mut b = chunk("B", ThreadStatus::Running);
        b.order = 1;
        b.depends_on = vec!["a".into()];
        let mut c = chunk("C", ThreadStatus::Running);
        c.order = 2;
        c.depends_on = vec!["a".into()];
        let mut d = chunk("D", ThreadStatus::Queued);
        d.order = 3;
        d.depends_on = vec!["b".into(), "c".into()];
        chunks.insert("a".into(), a);
        chunks.insert("b".into(), b);
        chunks.insert("c".into(), c);
        chunks.insert("d".into(), d);
        let s = snapshot(Some(TurnType::Execute), chunks);
        let m = build_mermaid(&s, &test_colors());
        // A has no deps -> edges from Plan.
        assert!(m.contains("stage_plan --> a"));
        // B and C depend on A.
        assert!(m.contains("a --> b"));
        assert!(m.contains("a --> c"));
        // D depends on B and C.
        assert!(m.contains("b --> d"));
        assert!(m.contains("c --> d"));
        // D has dependents (nothing depends on it is false here; nothing
        // depends on D so D -> Integrator).
        assert!(m.contains("d --> stage_integrator"));
        // A is depended on, so it does NOT directly edge into Integrator.
        assert!(!m.contains("a --> stage_integrator"));
    }

    #[test]
    fn active_stage_gets_active_fill() {
        let c = test_colors();
        let s = snapshot(Some(TurnType::Tpm), BTreeMap::new());
        let m = build_mermaid(&s, &c);
        assert!(m.contains(&format!("style stage_tpm fill:{}", c.node_active)));
        assert!(m.contains(&format!("style stage_scout fill:{}", c.node_done)));
        assert!(m.contains(&format!("style stage_plan fill:{}", c.node_default)));
    }

    #[test]
    fn done_chunk_gets_done_fill() {
        let c = test_colors();
        let mut chunks = BTreeMap::new();
        chunks.insert("c1".into(), chunk("First", ThreadStatus::Done));
        let s = snapshot(Some(TurnType::Execute), chunks);
        let m = build_mermaid(&s, &c);
        assert!(m.contains(&format!("style c1 fill:{}", c.node_done)));
    }

    #[test]
    fn sanitize_id_replaces_hyphens() {
        assert_eq!(sanitize_id("write-tree-command"), "write_tree_command");
    }

    #[test]
    fn cache_key_changes_with_source_or_dims() {
        let c = test_colors();
        let k1 = cache_key("a", 10, 10, &c);
        let k2 = cache_key("a", 10, 10, &c);
        let k3 = cache_key("b", 10, 10, &c);
        let k4 = cache_key("a", 20, 10, &c);
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
    }

    /// Regression: a snapshot with running chunks must produce a renderable
    /// mermaid diagram (the `\\n` injected into running chunk labels must not
    /// break the SVG render and the PNG must be non-empty).
    #[test]
    fn render_png_succeeds_for_running_chunk() {
        let c = test_colors();
        let mut chunks = BTreeMap::new();
        chunks.insert(
            "c1".into(),
            FlowSnapshotChunk {
                title: "Core object format".into(),
                status: ThreadStatus::Running,
                stage: Some(TurnType::Execute),
                ..Default::default()
            },
        );
        let s = snapshot(Some(TurnType::Execute), chunks);
        let source = build_mermaid(&s, &c);
        let png = render_png(&source, (400, 300), &c);
        match &png {
            Ok(bytes) => assert!(!bytes.is_empty(), "png should be non-empty"),
            Err(e) => panic!("render_png failed: {e}"),
        }
    }

    /// Regression for bug 4 (black image after chunk execution): the rasterized
    /// PNG must contain actual diagram content (non-background pixels), not
    /// just the fill color. Verifies resvg actually drew something into the
    /// pixmap for a multi-chunk vertical flowchart.
    #[test]
    fn render_png_has_content_for_multi_chunk_vertical() {
        let c = test_colors();
        let mut chunks = BTreeMap::new();
        for i in 1..=4 {
            chunks.insert(
                format!("c{i}"),
                FlowSnapshotChunk {
                    title: format!("Chunk number {i}"),
                    status: if i == 1 {
                        ThreadStatus::Running
                    } else {
                        ThreadStatus::Queued
                    },
                    stage: None,
                    ..Default::default()
                },
            );
        }
        let s = snapshot(Some(TurnType::Execute), chunks);
        let source = build_mermaid(&s, &c);
        let png = render_png(&source, (800, 600), &c).expect("render should succeed");
        let img = image::load_from_memory(&png).expect("png should decode");
        let rgba = img.to_rgba8();
        let bg = parse_hex(&c.background).expect("test bg is valid hex");
        let bg_pixel = [bg.0, bg.1, bg.2, 0xff];
        let content_pixels = rgba.pixels().filter(|p| p.0 != bg_pixel).count();
        assert!(
            content_pixels > 100,
            "expected diagram content but got only {content_pixels} non-bg pixels (black image bug)"
        );
    }
}
