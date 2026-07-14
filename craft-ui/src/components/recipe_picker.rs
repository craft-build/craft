use std::path::PathBuf;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use craft_agent::discovery::Discovery;
use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

const TITLE: &str = " Recipes ";
const FOOTER_HINTS: &[(&str, &str)] = &[("Enter", "run"), ("Esc", "close")];

pub enum RecipePickerAction {
    Consumed,
    Select(PathBuf),
    Close,
}

struct RecipeEntry {
    name: String,
    description: String,
    path: PathBuf,
}

impl PickerItem for RecipeEntry {
    fn label(&self) -> &str {
        &self.name
    }
    fn detail(&self) -> Option<&str> {
        if self.description.is_empty() {
            None
        } else {
            Some(&self.description)
        }
    }
}

pub struct RecipePicker {
    picker: ListPicker<RecipeEntry>,
}

impl RecipePicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER_HINTS),
        }
    }

    pub fn open(&mut self) {
        let discovery = Discovery::from_env();
        let files = discovery.discover_files("recipes", &["yaml", "yml", "json"]);
        let entries: Vec<RecipeEntry> = files
            .iter()
            .filter_map(|f| {
                let recipe = craft_agent::recipe::load(&f.path).ok()?;
                let name = recipe.name.clone().unwrap_or_else(|| f.name.clone());
                let description = recipe.description.clone().unwrap_or_default();
                Some(RecipeEntry {
                    name,
                    description,
                    path: f.path.clone(),
                })
            })
            .collect();
        self.picker.open(entries, TITLE);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> RecipePickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed => RecipePickerAction::Consumed,
            PickerAction::Select(_, entry) => RecipePickerAction::Select(entry.path),
            PickerAction::Close => RecipePickerAction::Close,
            PickerAction::Toggle(..) => RecipePickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for RecipePicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }
    fn close(&mut self) {
        self.close();
    }
}
