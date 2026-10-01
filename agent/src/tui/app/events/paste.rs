use super::*;

impl App {
    /// Paste lands in the composer as one unit; modal inputs own the
    /// keyboard so pastes are dropped while one is open. An open permission
    /// prompt's guidance buffer takes precedence.
    pub fn insert_paste(&mut self, text: &str) {
        // Same owner stack as `handle_key`: a prompt that declines the
        // paste (not in its editing buffer) lets it keep walking down.
        let mut owner = self.keyboard_owner();
        loop {
            match owner {
                KeyboardOwner::Modal => return,
                KeyboardOwner::PermissionPrompt => {
                    if self.overlays.permission_prompt.handle_paste(text) {
                        return;
                    }
                    owner = if self.overlays.question_form.is_open() {
                        KeyboardOwner::QuestionForm
                    } else {
                        self.owner_below()
                    };
                }
                KeyboardOwner::QuestionForm => {
                    if self.overlays.question_form.handle_paste(text) {
                        return;
                    }
                    owner = self.owner_below();
                }
                KeyboardOwner::Search => {
                    self.overlays.search.insert_paste(text);
                    self.refresh_search_matches();
                    return;
                }
                KeyboardOwner::FilePicker => {
                    self.overlays.file_picker.handle_paste(text);
                    return;
                }
                // The plan form takes no pastes; they reach the composer.
                KeyboardOwner::PlanForm | KeyboardOwner::Base => {
                    // A paste that is nothing but an image path attaches
                    // the image instead of inserting the text (F.6).
                    if let Some((path, media)) = crate::tui::ui::image::try_parse_image_path(text)
                        && path.is_file()
                    {
                        self.start_file_image_paste(path, media);
                        return;
                    }
                    self.composer.insert_paste(text);
                    self.overlays.slash_selected = 0;
                    return;
                }
            }
        }
    }

    /// Insert a picked path into the composer (reference: paste with
    /// spaces — a space separates it from existing text).
    pub(super) fn insert_path_into_composer(&mut self, path: &str) {
        if !self.composer.text.is_empty() && !self.composer.text.ends_with(' ') {
            self.composer.insert_char(' ');
        }
        self.composer.insert_paste(path);
    }
}
