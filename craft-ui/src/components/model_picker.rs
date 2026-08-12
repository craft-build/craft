use std::sync::Arc;

use arc_swap::ArcSwapOption;
use craft_providers::ModelTier;
use craft_providers::dynamic;
use craft_providers::model_registry;
use craft_providers::provider::ProviderKind;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::theme;

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.tool_dim),
        Span::styled("  !", t.keybind_key),
        Span::styled(" strong", t.tool_dim),
        Span::styled("  @", t.keybind_key),
        Span::styled(" medium", t.tool_dim),
        Span::styled("  #", t.keybind_key),
        Span::styled(" weak", t.tool_dim),
        Span::styled("  $", t.keybind_key),
        Span::styled(" compaction", t.tool_dim),
        Span::styled("  Tab", t.keybind_key),
        Span::styled(" switch provider", t.tool_dim),
    ])
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    let digit = match (key.code, key.modifiers.contains(KeyModifiers::SHIFT)) {
        // Kitty protocol: Shift+digit reported with base key + SHIFT modifier
        (KeyCode::Char(c @ '1'..='4'), true) => c,
        // Legacy terminals: Shift+digit reported as the resulting character
        (KeyCode::Char('!' | '¡'), false) => '1', // US, ES
        (KeyCode::Char('@' | '"' | '™'), false) => '2', // US, UK/DE
        (KeyCode::Char('#' | '§' | '£'), false) => '3', // US, DE, UK
        (KeyCode::Char('$' | '€' | '¤'), false) => '4', // US, EU, Nordic
        _ => return None,
    };
    match digit {
        '1' => Some(ModelTier::Strong),
        '2' => Some(ModelTier::Medium),
        '3' => Some(ModelTier::Weak),
        '4' => Some(ModelTier::Compaction),
        _ => None,
    }
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    Close,
}

#[derive(Clone)]
struct ModelEntry {
    spec: String,
    id: String,
    provider_display: String,
    suffix: Option<String>,
    tier: String,
    override_tiers: Vec<ModelTier>,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.tier)
    }

    fn section(&self) -> Option<&str> {
        None
    }

    fn is_highlighted(&self) -> bool {
        !self.override_tiers.is_empty()
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    recents: Vec<String>,
    current_spec: String,
    last_spec_count: usize,
    dirty: bool,
    /// User-moved entry to restore on refresh: `(tab_title, spec)`.
    anchor: Option<(String, String)>,
    tabs: Vec<Tab>,
    active_tab: usize,
}

struct Tab {
    title: String,
    entries: Vec<ModelEntry>,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new()
                .with_footer_builder(footer_line)
                .with_header(1),
            models,
            recents: Vec::new(),
            current_spec: String::new(),
            last_spec_count: 0,
            dirty: false,
            anchor: None,
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.dirty = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.current_spec = current_spec.to_owned();
        self.anchor = None;
        self.dirty = false;
        self.tabs = self.load_entries();
        self.picker.open(self.active_entries(), TITLE);
        self.preselect_current_model();
    }

    fn try_refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.models.load();
        let spec_count = guard.as_deref().map_or(0, Vec::len);
        if spec_count == self.last_spec_count && !self.dirty {
            return;
        }
        drop(guard);
        self.dirty = false;
        self.tabs = self.load_entries();
        let (tab, spec) = match self.anchor.clone() {
            Some((tab_title, spec)) => (
                self.tabs
                    .iter()
                    .position(|t| t.title == tab_title && t.entries.iter().any(|e| e.spec == spec))
                    .or_else(|| {
                        self.tabs
                            .iter()
                            .position(|t| t.entries.iter().any(|e| e.spec == spec))
                    })
                    .unwrap_or(0),
                spec,
            ),
            None => (self.locate_current_model_tab(), self.current_spec.clone()),
        };
        self.active_tab = tab;
        self.picker.replace_items(self.active_entries());
        self.picker.select_item_by(|e| e.spec == spec);
    }

    fn active_entries(&self) -> Vec<ModelEntry> {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.entries.clone())
            .unwrap_or_default()
    }

    fn preselect_current_model(&mut self) {
        self.active_tab = self.locate_current_model_tab();
        self.picker.replace_items(self.active_entries());
        self.picker.select_item_by(|e| e.spec == self.current_spec);
    }

    fn locate_current_model_tab(&self) -> usize {
        self.tabs
            .iter()
            .position(|t| {
                t.title != RECENT_SECTION && t.entries.iter().any(|e| e.spec == self.current_spec)
            })
            .or_else(|| {
                self.tabs
                    .iter()
                    .position(|t| t.entries.iter().any(|e| e.spec == self.current_spec))
            })
            .unwrap_or(0)
    }

    fn load_entries(&mut self) -> Vec<Tab> {
        let guard = self.models.load();
        let specs = guard.as_deref();
        self.last_spec_count = specs.map_or(0, Vec::len);

        let mut tabs: Vec<Tab> = Vec::new();

        let recent_specs = self.recents.clone();
        if !recent_specs.is_empty() {
            let mut entries: Vec<ModelEntry> = Vec::new();
            for spec in &recent_specs {
                if let Some(mut e) = parse_model_entry(spec) {
                    e.suffix = Some(std::mem::take(&mut e.provider_display));
                    e.provider_display = RECENT_SECTION.to_string();
                    entries.push(e);
                }
            }
            if !entries.is_empty() {
                tabs.push(Tab {
                    title: RECENT_SECTION.to_string(),
                    entries,
                });
            }
        }

        let full: Vec<ModelEntry> = specs
            .map(|s| s.iter().filter_map(|s| parse_model_entry(s)).collect())
            .unwrap_or_default();

        let mut provider_tabs: Vec<(String, Vec<ModelEntry>)> = Vec::new();
        for entry in full {
            if let Some(slot) = provider_tabs
                .iter_mut()
                .find(|(name, _)| *name == entry.provider_display)
            {
                slot.1.push(entry);
            } else {
                provider_tabs.push((entry.provider_display.clone(), vec![entry]));
            }
        }
        provider_tabs.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entries) in provider_tabs {
            tabs.push(Tab {
                title: name,
                entries,
            });
        }

        tabs
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn set_keybindings(
        &mut self,
        resolver: std::sync::Arc<crate::components::keybindings::KeybindingResolver>,
    ) {
        self.picker.set_keybindings(resolver);
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.track_anchor(|p| p.picker.handle_paste(text))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.track_anchor(|p| p.handle_key_inner(key))
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> ModelPickerAction {
        if self.tabs.len() > 1 {
            match key.code {
                KeyCode::Tab => {
                    self.switch_tab(1);
                    return ModelPickerAction::Consumed;
                }
                KeyCode::BackTab => {
                    self.switch_tab(-1);
                    return ModelPickerAction::Consumed;
                }
                _ => {}
            }
        }
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            let spec = entry.spec.clone();
            self.dirty = true;
            return if entry.override_tiers.contains(&tier) {
                ModelPickerAction::UnassignTier(spec, tier)
            } else {
                ModelPickerAction::AssignTier(spec, tier)
            };
        }
        match self.picker.handle_key(key) {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(entry) => ModelPickerAction::Select(entry.spec),
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    fn track_anchor<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let before_tab = self.active_tab;
        let before_idx = self.picker.selected_index();
        let result = f(self);
        let after_tab = self.active_tab;
        let after_idx = self.picker.selected_index();
        if before_tab != after_tab || before_idx != after_idx {
            self.anchor = self.current_anchor();
        }
        result
    }

    fn current_anchor(&self) -> Option<(String, String)> {
        let tab = self.tabs.get(self.active_tab)?;
        let entry = self.picker.selected_item()?;
        tab.entries
            .iter()
            .any(|e| e.spec == entry.spec)
            .then(|| (tab.title.clone(), entry.spec.clone()))
    }

    fn switch_tab(&mut self, delta: i32) {
        if self.tabs.len() <= 1 {
            return;
        }
        let len = self.tabs.len() as i32;
        self.active_tab = ((self.active_tab as i32 + delta).rem_euclid(len)) as usize;
        self.picker.clear_search();
        self.picker.replace_items(self.active_entries());
        self.picker.select_item_by(|e| e.spec == self.current_spec);
        if let Some(anchor) = self.current_anchor() {
            self.anchor = Some(anchor);
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.try_refresh();
        let popup = self.picker.view(frame, area);
        if let Some(header) = self.picker.header_area() {
            render_tabs(frame, header, &self.tabs, self.active_tab);
        }
        popup
    }
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name().to_string()
    } else if let Some(name) = dynamic::display_name(provider_str) {
        name.to_string()
    } else {
        let config = craft_config::providers::ProvidersConfig::load();
        config.get(provider_str)?;
        craft_config::providers::resolve_display_name(provider_str, config.get(provider_str))
    };

    let map = model_registry::model_registry().read().unwrap();
    let override_tiers: Vec<ModelTier> = [
        ModelTier::Strong,
        ModelTier::Medium,
        ModelTier::Weak,
        ModelTier::Compaction,
    ]
    .into_iter()
    .filter(|&t| map.has_override(spec, t))
    .collect();
    let override_label = map.override_tier_label(spec);
    drop(map);
    let tier = override_label.unwrap_or_else(|| match craft_providers::Model::from_spec(spec) {
        Ok(m) => m.tier.to_string(),
        Err(_) => String::new(),
    });
    Some(ModelEntry {
        spec: spec.to_string(),
        id: model_id.to_string(),
        provider_display,
        suffix: None,
        tier,
        override_tiers,
    })
}

fn render_tabs(frame: &mut Frame, area: Rect, tabs: &[Tab], active: usize) {
    let t = theme::current();
    let mut spans: Vec<Span> = Vec::with_capacity(tabs.len() * 2);
    spans.push(Span::raw(" "));
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let style = if i == active {
            t.item_selected
        } else {
            t.keybind_section
        };
        spans.push(Span::styled(tab.title.as_str(), style));
    }
    let mut line = Line::from(spans);
    let width = area.width as usize;
    if line.width() > width {
        let target = width.saturating_sub(1);
        let mut buf = String::new();
        let mut w = 0;
        for span in &line.spans {
            for ch in span.content.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > target {
                    break;
                }
                w += cw;
                buf.push(ch);
            }
            if w > target {
                break;
            }
        }
        buf.push('\u{2026}');
        line = Line::raw(buf);
    }
    frame.render_widget(Paragraph::new(vec![line]), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        models
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.tier.is_empty());
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test_case(key(KeyCode::Char('!')),           ModelTier::Strong     ; "legacy_bang_strong")]
    #[test_case(key(KeyCode::Char('¡')),           ModelTier::Strong     ; "legacy_inverted_bang_strong")]
    #[test_case(key(KeyCode::Char('@')),           ModelTier::Medium     ; "legacy_at_medium")]
    #[test_case(key(KeyCode::Char('"')),           ModelTier::Medium     ; "legacy_quote_medium")]
    #[test_case(key(KeyCode::Char('™')),           ModelTier::Medium     ; "legacy_tm_medium")]
    #[test_case(key(KeyCode::Char('#')),           ModelTier::Weak       ; "legacy_hash_weak")]
    #[test_case(key(KeyCode::Char('§')),           ModelTier::Weak       ; "legacy_section_weak")]
    #[test_case(key(KeyCode::Char('£')),           ModelTier::Weak       ; "legacy_pound_weak")]
    #[test_case(key(KeyCode::Char('$')),           ModelTier::Compaction ; "legacy_dollar_compaction")]
    #[test_case(key(KeyCode::Char('€')),           ModelTier::Compaction ; "legacy_euro_compaction")]
    #[test_case(key(KeyCode::Char('¤')),           ModelTier::Compaction ; "legacy_currency_compaction")]
    #[test_case(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT), ModelTier::Strong     ; "kitty_shift_1_strong")]
    #[test_case(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::SHIFT), ModelTier::Medium     ; "kitty_shift_2_medium")]
    #[test_case(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::SHIFT), ModelTier::Weak       ; "kitty_shift_3_weak")]
    #[test_case(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::SHIFT), ModelTier::Compaction ; "kitty_shift_4_compaction")]
    fn tier_shortcut_assigns_and_keeps_picker_open(k: KeyEvent, want: ModelTier) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(p.is_open());
    }

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101"),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101");

        p.picker.select(0);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-sonnet-4-20250514"),
            "opening on anthropic model lands on anthropic tab, first model is sonnet",
        );

        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected in its provider tab",
        );
        assert_eq!(
            p.tabs[p.active_tab].title,
            zai_display_name(&p),
            "selection should land on the provider tab, not the Recent copy",
        );
    }

    fn multi_provider_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    fn zai_display_name(p: &ModelPicker) -> &str {
        p.tabs
            .iter()
            .find(|t| t.entries.iter().any(|e| e.spec == "zai/glm-5"))
            .map(|t| t.title.as_str())
            .expect("zai tab should exist")
    }

    #[test]
    fn tabs_built_in_display_order() {
        let mut p = ModelPicker::new(multi_provider_models());
        p.open("");

        assert_eq!(p.tabs.len(), 2, "one tab per provider");
        let titles: Vec<&str> = p.tabs.iter().map(|t| t.title.as_str()).collect();
        assert!(titles[0] < titles[1], "provider tabs sorted alphabetically");
        assert!(titles.contains(&"Anthropic"));
    }

    #[test]
    fn tabs_include_recent_first_when_present() {
        let models = multi_provider_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec!["zai/glm-5".into()]);
        p.open("");

        assert!(p.tabs.len() >= 2);
        assert_eq!(p.tabs[0].title, "Recent", "Recent tab is always first");
    }

    #[test]
    fn open_lands_on_current_model_provider_tab() {
        let mut p = ModelPicker::new(multi_provider_models());

        p.open("zai/glm-5");
        let zai_title = zai_display_name(&p).to_string();
        assert_eq!(p.tabs[p.active_tab].title, zai_title);

        p.open("anthropic/claude-sonnet-4-20250514");
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");
    }

    #[test]
    fn tab_key_advances_and_wraps() {
        let mut p = ModelPicker::new(multi_provider_models());
        p.open("anthropic/claude-sonnet-4-20250514");
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");
        let other_title = p.tabs[1].title.clone();

        let action = p.handle_key(key(KeyCode::Tab));
        assert!(matches!(action, ModelPickerAction::Consumed));
        assert_eq!(p.tabs[p.active_tab].title, other_title);

        let action = p.handle_key(key(KeyCode::Tab));
        assert!(matches!(action, ModelPickerAction::Consumed));
        assert_eq!(
            p.tabs[p.active_tab].title, "Anthropic",
            "Tab should wrap to the first provider"
        );
    }

    #[test]
    fn backtab_key_retreats_and_wraps() {
        let mut p = ModelPicker::new(multi_provider_models());
        p.open("anthropic/claude-sonnet-4-20250514");
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");
        let other_title = p.tabs[1].title.clone();

        let action = p.handle_key(key(KeyCode::BackTab));
        assert!(matches!(action, ModelPickerAction::Consumed));
        assert_eq!(
            p.tabs[p.active_tab].title, other_title,
            "BackTab should wrap to the last provider"
        );

        let action = p.handle_key(key(KeyCode::BackTab));
        assert!(matches!(action, ModelPickerAction::Consumed));
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");
    }

    #[test]
    fn tab_switch_clears_search() {
        let mut p = ModelPicker::new(multi_provider_models());
        p.open("anthropic/claude-sonnet-4-20250514");

        p.handle_key(key(KeyCode::Char('s')));
        p.handle_key(key(KeyCode::Tab));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "after switching tabs with cleared search, first model of new tab is selected",
        );
    }

    #[test]
    fn tab_switch_selects_current_model_if_present() {
        let mut p = ModelPicker::new(multi_provider_models());
        p.open("zai/glm-5");

        p.handle_key(key(KeyCode::Tab));
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");

        p.handle_key(key(KeyCode::Tab));
        assert!(
            p.tabs[p.active_tab]
                .entries
                .iter()
                .any(|e| e.spec == "zai/glm-5"),
            "second Tab returns to the current model's tab"
        );

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "switching back to the current model's tab should reselect it",
        );
    }

    #[test]
    fn tab_switch_wraps_correct_count() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        let mut p = ModelPicker::new(models);
        p.open("");

        let start = p.active_tab;
        for _ in 0..p.tabs.len() {
            p.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            p.active_tab, start,
            "cycling through all tabs should return to the starting tab"
        );
    }

    #[test]
    fn refresh_clamps_active_tab_when_provider_disappears() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("zai/glm-5");
        assert!(p.active_tab < p.tabs.len());

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        p.try_refresh();

        assert!(
            p.active_tab < p.tabs.len(),
            "active tab should be clamped within bounds after refresh"
        );
        assert_eq!(p.tabs[p.active_tab].title, "Anthropic");
    }

    #[test]
    fn tab_key_does_nothing_with_single_provider() {
        let mut p = ModelPicker::new(test_models());
        p.open("");

        let initial_tab = p.active_tab;
        let action = p.handle_key(key(KeyCode::Tab));
        assert!(matches!(action, ModelPickerAction::Consumed));
        assert_eq!(p.active_tab, initial_tab);
    }

    #[test]
    fn reopen_preselects_current_model_in_provider_tab() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");
        assert_eq!(
            p.tabs[p.active_tab].title, "Anthropic",
            "open should land on the provider tab, not the Recent copy",
        );

        p.open("zai/glm-5");
        assert_eq!(p.tabs[p.active_tab].title, "Z.AI");
        let entry = p.picker.selected_item().expect("selection on reopen");
        assert_eq!(entry.spec, "zai/glm-5");
    }

    #[test]
    fn refresh_keeps_selection_on_provider_entry() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        assert_eq!(p.tabs[p.active_tab].title, "Z.AI");
        p.handle_key(key(KeyCode::Char('!')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(
            p.tabs[p.active_tab].title, "Z.AI",
            "selection should stay on the provider tab, not jump to Recent",
        );
    }

    #[test]
    fn refresh_after_collapse_anchors_to_provider_tab() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        models.store(None);
        p.try_refresh();
        let entry = p.picker.selected_item().expect("selection during collapse");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(p.tabs[p.active_tab].title, "Recent");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let entry = p.picker.selected_item().expect("selection after arrival");
        assert_eq!(entry.spec, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(
            p.tabs[p.active_tab].title, "Anthropic",
            "cursor should migrate to the provider tab once it arrives",
        );
    }

    #[test]
    fn refresh_preserves_navigation_to_recent_tab() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-sonnet-4-20250514");

        p.handle_key(key(KeyCode::BackTab));
        assert_eq!(p.tabs[p.active_tab].title, "Recent");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(
            p.tabs[p.active_tab].title, "Recent",
            "user navigation to the Recent tab should survive refresh",
        );
        assert!(
            p.tabs[p.active_tab]
                .entries
                .iter()
                .any(|e| e.spec == entry.spec),
            "selected entry must belong to the active tab",
        );
    }

    #[test]
    fn refresh_preserves_selection_with_active_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "zai/glm-5".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        assert_eq!(p.tabs[p.active_tab].title, "Z.AI");
        p.handle_key(key(KeyCode::Char('g')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('m')));

        models.store(None);
        p.try_refresh();
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let entry = p.picker.selected_item().expect("selection after refresh");
        assert_eq!(entry.spec, "zai/glm-5");
        assert_eq!(p.tabs[p.active_tab].title, "Z.AI");
    }
}
