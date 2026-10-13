//! Layout composition: one chat column (messages / forms / todo widget /
//! composer / status rows), with overlays on top. Painting lives in
//! [`frame::frame` (the slt path)](`crate::tui::ui::frame::frame`); this
//! module owns the panel tree.

pub(crate) mod composer;
pub mod frame;
pub mod image;
pub(crate) mod messages;
mod overlays;
pub mod scrollback;
pub mod theme;
mod todo;

#[cfg(test)]
mod tests {
    use crate::tui::app::App;
    use crate::tui::provider::{
        AgentEvent, LineKind, ModelChoice, PlanItem, Status, Tone, ToolCallData, ToolKind,
        ToolLine, TouchedFile,
    };
    use crate::tui::style::Modifiers;
    use crate::tui::ui::frame::{self, frame};
    use crate::tui::ui::theme;

    /// The app rendered on an `slt::TestBackend`, mirroring the run loop:
    /// each `draw` is one frame, and the layout handoff (widths, scroll
    /// bounds) settles over the first two frames.
    struct Screen {
        tb: slt::TestBackend,
        view: frame::View,
    }

    impl Screen {
        fn new(width: u32, height: u32) -> Self {
            Self {
                tb: slt::TestBackend::new(width, height),
                view: frame::View::default(),
            }
        }

        fn draw(&mut self, app: &mut App) {
            self.tb.render(|ui| frame(ui, app, &mut self.view));
        }

        /// Two frames, so the transcript's prev-frame layout reads settle
        /// before the assertions look at the buffer.
        fn draw_settled(&mut self, app: &mut App) {
            self.draw(app);
            self.draw(app);
        }

        fn rows(&self) -> Vec<String> {
            (0..self.tb.height()).map(|y| self.tb.line(y)).collect()
        }

        fn text(&self) -> String {
            self.rows().join("\n")
        }
    }

    fn seeded_app() -> App {
        let mut app = App::new();
        app.handle_event(AgentEvent::PlanSet(vec![
            PlanItem {
                label: "Read the refresh token path".into(),
                done: true,
                active: false,
            },
            PlanItem {
                label: "Guard refreshToken() with a mutex".into(),
                done: false,
                active: true,
            },
        ]));
        app.handle_event(AgentEvent::FilesSet(vec![TouchedFile {
            path: "src/auth/refresh.ts".into(),
            status: "modified".into(),
            tone: Tone::Warning,
        }]));
        app.handle_event(AgentEvent::StatusChanged(Status::Done));
        app.handle_event(AgentEvent::TokenUsage("44.8K/1M (4%)".into()));
        app.handle_event(AgentEvent::AssistantText(
            "Looking at the refresh path first.".into(),
        ));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            awaiting_approval: false,
            image: None,
            kind: ToolKind::Read {
                path: "src/auth/refresh.ts".into(),
                summary: "38 lines".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Context,
                text: "export async function refreshToken() {".into(),
                ..Default::default()
            }],
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t2".into(),
            awaiting_approval: true,
            image: None,
            kind: ToolKind::Edit {
                path: "src/auth/refresh.ts".into(),
                summary: String::new(),
            },
            lines: vec![
                ToolLine {
                    kind: LineKind::Del,
                    text: "  session.token = res.token".into(),
                    nr: 3,
                    ..Default::default()
                },
                ToolLine {
                    kind: LineKind::Add,
                    text: "  if (inflight) return inflight".into(),
                    ..Default::default()
                },
            ],
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t3".into(),
            awaiting_approval: false,
            image: None,
            kind: ToolKind::Bash {
                cmd: "pnpm test auth/refresh.spec.ts".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Success,
                text: "✓ all 6 tests passed".into(),
                ..Default::default()
            }],
        }));
        app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
        app
    }

    #[test]
    fn help_sheet_renders_grouped_contexts_and_effective_chords() {
        use crate::tui::keybindings::KeybindingResolver;
        use crate::tui::modals::Modal;
        let mut screen = Screen::new(100, 40);
        let mut app = seeded_app();
        // A user override: search moves to Alt+M and must render that way.
        let entries = vec![("search".to_string(), vec!["Alt+M".to_string()])];
        let mut warnings = Vec::new();
        app.overlays.keybinds = KeybindingResolver::from_overlay(&entries, &mut warnings);
        app.overlays.modal = Modal::Help;
        screen.draw_settled(&mut app);
        let all = screen.text();
        assert!(all.contains("Keybindings"), "title rendered");
        assert!(all.contains("General"), "section header rendered");
        assert!(all.contains("Editing"), "section header rendered");
        assert!(all.contains("Alt+M"), "overridden chord rendered");
        assert!(
            !all.contains("Ctrl+F /"),
            "default search chord replaced, not duplicated"
        );
        assert!(all.contains("esc to close"), "footer rendered");
    }

    /// The permission prompt renders in the composer box's place, styled
    /// like it: warning-colored bar, padded panel, the tool call and its
    /// key hints — the todo widget above it, the status bar below, and no
    /// input box (or input selection region) while it is up.
    #[test]
    fn permission_prompt_replaces_the_composer() {
        let mut screen = Screen::new(100, 30);
        let mut app = seeded_app();
        app.handle_event(AgentEvent::PermissionRequest {
            id: "t9".into(),
            tool: "bash".into(),
            scopes: vec!["execute".into()],
            files: Vec::new(),
            commands: vec!["rm -rf /tmp/x".into()],
        });
        app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
        screen.draw_settled(&mut app);
        let rows = screen.rows();

        let title = rows
            .iter()
            .position(|r| r.contains("Permission Required"))
            .expect("prompt title rendered");
        assert!(
            rows[title].starts_with("  ▎ Permission Required"),
            "{:?}",
            rows[title]
        );
        assert!(
            rows[title + 2].contains("bash"),
            "tool row: {:?}",
            rows[title + 2]
        );
        assert!(
            rows[title + 3].contains("rm -rf /tmp/x"),
            "cmd row: {:?}",
            rows[title + 3]
        );
        let t = theme::current();
        assert_eq!(
            screen.tb.buffer().get(2, title as u32).style.fg,
            Some(t.warning),
            "warning-colored bar"
        );

        // The todo widget sits above the prompt; no input box anywhere.
        let todo = rows.iter().position(|r| r.contains("todo ")).expect("todo");
        assert!(todo < title, "todo widget above the prompt");
        assert!(!screen.text().contains("Message Craft…"), "input hidden");
        assert!(
            rows.last().unwrap().contains("tab to switch"),
            "status bar kept"
        );
        assert_eq!(
            app.view.paint.lock().unwrap().composer_area,
            crate::tui::geom::Rect::default()
        );
    }

    #[test]
    fn composer_info_line_carries_model_provider_and_context_usage() {
        // Wide enough that the bar sheds nothing.
        let mut screen = Screen::new(140, 24);
        let mut app = seeded_app();
        app.composer.clear();
        app.handle_event(AgentEvent::CatalogSet {
            models: vec![ModelChoice {
                provider: "anthropic".into(),
                model: "claude-sonnet-5".into(),
                label: "claude-sonnet-5".into(),
                provider_label: "anthropic".into(),
                thinking_choices: crate::thinking::ThinkingConfig::choices(),
            }],
            current: 0,
        });
        screen.draw_settled(&mut app);
        let rows = screen.rows();
        let bottom: Vec<String> = rows[rows.len() - 4..].to_vec();
        // The composer box: the text row between blank padding rows, the
        // accent bar down all three.
        for (i, r) in bottom[..3].iter().enumerate() {
            assert!(r.starts_with("  ▎"), "box row {i}: {r:?}");
        }
        assert!(bottom[1].contains("▎ >"), "prompt row: {:?}", bottom[1]);
        // The status bar: model · thinking │ mode chip · tab hint, the
        // spinner right of the hint, the context meter right-aligned.
        let bar = &bottom[3];
        assert!(bar.contains("● claude-sonnet-5 · off"), "model: {bar:?}");
        assert!(!bar.contains("anthropic"), "no provider: {bar:?}");
        assert!(bar.contains("│   BUILD  · tab to switch"), "mode: {bar:?}");
        let hint = bar.find("tab to switch").unwrap();
        let spin = bar.find("awaiting approval").expect("spinner");
        assert!(spin > hint, "spinner right of the tab hint: {bar:?}");
        assert!(bar.contains("esc interrupt"), "{bar:?}");
        assert!(
            bar.trim_end().ends_with("▰ 44.8K / 1M 4%"),
            "meter: {bar:?}"
        );
    }

    #[test]
    fn thinking_picker_renders_modes_and_current_budget() {
        let mut screen = Screen::new(90, 24);
        let mut app = seeded_app();
        app.session.thinking = crate::thinking::ThinkingConfig::Budget(4096);
        app.open_thinking_picker();
        screen.draw_settled(&mut app);
        let text = screen.text();
        for label in [
            "Thinking", "off", "adaptive", "minimal", "low", "medium", "high", "xhigh", "max",
            "● 4096",
        ] {
            assert!(text.contains(label), "missing {label:?} in:\n{text}");
        }
        assert!(text.contains("/thinking <tokens>"));
        let mut small = Screen::new(30, 12);
        small.draw_settled(&mut app);
    }

    #[test]
    fn overlays_render_without_panic() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();

        app.overlays.modal = crate::tui::modals::Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("Change model"));

        app.overlays.modal = crate::tui::modals::Modal::ModelMenu(2);
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("Claude Opus 4.1"));

        app.overlays.modal = crate::tui::modals::Modal::None;
        app.composer.text = "/cl".into();
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("/clear"));

        // Theme picker: title, windowed list, and the current-theme marker.
        let entries = theme::all_theme_names();
        let selected = entries.len().saturating_sub(1); // last row: forces windowing
        app.overlays.modal = crate::tui::modals::Modal::ThemePicker {
            entries,
            selected,
            original: theme::DEFAULT_THEME.to_owned(),
        };
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("Themes"));
        assert!(text.contains("zenburn")); // last entry stays visible via the window
    }

    /// The palette scrolls its window so the highlighted item stays visible
    /// past the eighth row (regression: the selection used to scroll off the
    /// rendered window and vanish).
    #[test]
    fn palette_scrolls_to_keep_selection_visible() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        app.overlays.modal = crate::tui::modals::Modal::Palette {
            query: String::new(),
            selected: 0,
        };
        let last = app.palette_items().len() - 1;
        let (_, label, _) = app.palette_items()[last];
        app.overlays.modal = crate::tui::modals::Modal::Palette {
            query: String::new(),
            selected: last,
        };
        screen.draw_settled(&mut app);
        assert!(
            screen.text().contains(label),
            "the selected row ({label}) must be rendered"
        );
    }

    #[test]
    fn usage_and_stats_overlays_render_rows_and_costs() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        app.handle_event(AgentEvent::UsageSnapshot(vec![
            crate::tui::provider::UsageRow {
                model: "anthropic/claude-sonnet-5".into(),
                tokens: 12_345,
                cost: Some(0.0123),
            },
            crate::tui::provider::UsageRow {
                model: "mock/free-model".into(),
                tokens: 500,
                cost: None,
            },
        ]));
        app.overlays.modal = crate::tui::modals::Modal::Usage(app.overlays.usage.clone());
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("Session usage"));
        assert!(text.contains("claude-sonnet-5"));
        assert!(text.contains("$0.01"), "priced model shows its cost");
        assert!(text.contains("\u{2014}"), "unpriced model shows an em dash");

        app.overlays.modal = crate::tui::modals::Modal::Stats(crate::tui::modals::StatsView {
            rows: vec![crate::tui::provider::UsageRow {
                model: "claude-sonnet-5".into(),
                tokens: 100_000,
                cost: Some(1.5),
            }],
            by_session: vec![("0123456789abcdef".into(), 1.5, 100_000)],
            models_overflow: 0,
            total_cost: 1.5,
            unpriced_records: 0,
            total_tokens: 100_000,
            sessions: 3,
            empty: false,
        });
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("Cost stats"));
        assert!(text.contains("$1.50"));
        assert!(text.contains("3 sessions"));

        app.overlays.modal = crate::tui::modals::Modal::Stats(crate::tui::modals::StatsView {
            empty: true,
            ..Default::default()
        });
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("no runs recorded"));
    }

    #[test]
    fn usage_overlay_renders_the_provider_quota_section() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        app.overlays.usage = vec![crate::tui::provider::UsageRow {
            model: "anthropic/claude-sonnet-5".into(),
            tokens: 12_345,
            cost: Some(0.0123),
        }];
        app.overlays.modal = crate::tui::modals::Modal::Usage(app.overlays.usage.clone());

        app.overlays.usage_quota =
            crate::tui::provider::UsageFetchState::Ready(crate::providers::ProviderUsage {
                plan: Some("lite".into()),
                limits: vec![crate::providers::UsageLimit {
                    label: "5-hour window".into(),
                    percentage: Some(14),
                    reset_at: Some(1_800_000_000_000),
                    detail: None,
                }],
                by_model_today: vec![crate::providers::ModelUsageRow {
                    model: "claude-sonnet-5".into(),
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                    spend_microdollars: 2_330_000,
                }],
            });
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("Provider quota (lite):"));
        assert!(text.contains("5-hour window"));
        assert!(text.contains("14%"));
        assert!(text.contains("resets "));
        assert!(text.contains("By model (provider, today):"));
        assert!(text.contains("$2.33 today"));

        app.overlays.usage_quota = crate::tui::provider::UsageFetchState::Loading;
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("fetching"));

        app.overlays.usage_quota = crate::tui::provider::UsageFetchState::Unsupported;
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("not available"));

        app.overlays.usage_quota =
            crate::tui::provider::UsageFetchState::Error("rate limited".into());
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("rate limited"));
    }

    #[test]
    fn stats_overlay_caps_models_and_lists_top_sessions() {
        let mut screen = Screen::new(120, 40);
        let mut app = seeded_app();
        // The view is capped at build time (sidebar::stats_view): 12 rows
        // in the view, 2 more beyond it.
        let rows: Vec<_> = (0..12)
            .map(|i| crate::tui::provider::UsageRow {
                model: format!("provider/model-{i:02}"),
                tokens: 1_000,
                cost: Some(0.1),
            })
            .collect();
        app.overlays.modal = crate::tui::modals::Modal::Stats(crate::tui::modals::StatsView {
            rows,
            by_session: vec![
                ("0123456789abcdef".into(), 1.5, 100_000),
                ("fedcba9876543210".into(), 0.5, 50_000),
            ],
            models_overflow: 2,
            total_cost: 2.0,
            unpriced_records: 0,
            total_tokens: 150_000,
            sessions: 2,
            empty: false,
        });
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("By model:"));
        assert!(!text.contains("model-12"), "only the 12 capped rows render");
        assert!(text.contains("+2 more models"));
        assert!(text.contains("Top sessions:"));
        assert!(
            text.contains("01234567"),
            "sessions render 8-char short ids"
        );
        assert!(text.contains("fedcba98"));
    }

    /// A catalog larger than the space above the composer windows around the
    /// selection instead of painting over the chat and composer rows.
    #[test]
    fn model_menu_windows_around_selection_for_large_catalogs() {
        let mut screen = Screen::new(120, 30);
        let mut app = seeded_app();
        let models: Vec<ModelChoice> = (0..30)
            .map(|i| ModelChoice {
                provider: "p".into(),
                model: format!("m{i:02}"),
                label: format!("m{i:02}"),
                provider_label: "p".into(),
                thinking_choices: crate::thinking::ThinkingConfig::choices(),
            })
            .collect();
        app.handle_event(AgentEvent::CatalogSet {
            models,
            current: 25,
        });
        app.overlays.modal = crate::tui::modals::Modal::ModelMenu(25);
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("m25"), "window keeps the selection visible");
        assert!(text.contains("m29"), "window extends to the catalog tail");
        assert!(!text.contains("m00"), "out-of-window rows are clipped");
        assert!(!text.contains("m01"), "out-of-window rows are clipped");
    }

    /// Rows show tiers held in the global registry, and the keybind footer
    /// advertises the tier-assignment keys (task 73).
    #[test]
    fn model_menu_shows_tier_labels_and_footer() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        // Seed: Opus holds strong+weak; Sonnet holds medium.
        crate::model_registry::set_and_persist(
            "Anthropic/Claude Opus 4.1".into(),
            crate::model_registry::ModelTier::Strong,
            &state,
        );
        crate::model_registry::set_and_persist(
            "Anthropic/Claude Opus 4.1".into(),
            crate::model_registry::ModelTier::Weak,
            &state,
        );
        app.overlays.modal = crate::tui::modals::Modal::ModelMenu(2);
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("strong/weak"), "multi-tier label: {text:?}");
        assert!(text.contains("! strong"), "footer hints: {text:?}");
        assert!(text.contains("$ compaction"), "footer hints: {text:?}");
    }

    #[test]
    fn selection_highlights_cells() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        // Drag right-to-left across part of the first assistant line (row 1).
        let region = crate::tui::geom::Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 36,
        };
        app.view.selection = Some(crate::tui::selection::Selection {
            anchor: (1, 12),
            head: (1, 4),
            region,
        });
        screen.draw_settled(&mut app);
        let buf = screen.tb.buffer();
        // Cells inside the range are reversed, outside are not.
        assert!(buf.get(4, 1).style.modifiers.contains(Modifiers::REVERSED));
        assert!(buf.get(12, 1).style.modifiers.contains(Modifiers::REVERSED));
        assert!(!buf.get(13, 1).style.modifiers.contains(Modifiers::REVERSED));
        // And the frame snapshot holds the selectable text.
        assert!(
            app.view.paint.lock().unwrap().frame_text[1].contains("Looking at the refresh path")
        );
    }

    #[test]
    fn status_indicator_reflects_and_animates_with_agent_status() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();

        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        screen.draw_settled(&mut app);
        let first = screen.text();
        assert!(first.contains("running"));

        // The spinner frame advances with the tick counter.
        screen.draw(&mut app);
        let second = screen.text();
        assert!(second.contains("running"));
        assert_ne!(first, second, "spinner frame changes between draws");

        app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("awaiting approval"));

        app.handle_event(AgentEvent::StatusChanged(Status::Failed));
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("failed"));
    }

    /// The activity row's indicator starts two cells in, lined up under the
    /// composer's accent bar rather than flush with the row's left edge.
    #[test]
    fn status_indicator_aligns_under_the_composer_accent_bar() {
        let mut screen = Screen::new(120, 36);
        let mut app = seeded_app();
        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        screen.draw_settled(&mut app);
        let row = screen
            .rows()
            .into_iter()
            .position(|r| r.contains("running"))
            .expect("status row with the running indicator");
        let buf = screen.tb.buffer();
        assert_eq!(buf.get(0, row as u32).symbol, " ");
        assert_eq!(buf.get(1, row as u32).symbol, " ");
        assert_ne!(
            buf.get(2, row as u32).symbol,
            " ",
            "indicator starts under the accent bar"
        );
    }

    #[test]
    fn narrow_terminal_renders_without_panicking() {
        let mut screen = Screen::new(30, 12);
        let mut app = seeded_app();
        app.plan = vec![PlanItem {
            label: "a long step label that cannot fit".into(),
            done: false,
            active: true,
        }];
        screen.draw_settled(&mut app);
        assert!(screen.text().contains("todo 0/1"));
    }

    /// The plan form (Ctrl-T) renders in the composer box's place: its menu
    /// items are visible in Plan mode with the input hidden and the status
    /// bar kept, and the dismissed state brings the input back and leaves
    /// the one-row " Plan Ctrl+T " hint in the footer.
    #[test]
    fn plan_form_replaces_composer_and_hints_when_dismissed() {
        let mut screen = Screen::new(100, 30);
        let mut app = App::new();
        app.mode = crate::tui::app::Mode::Plan;
        app.plan_mode.plan_path = Some(std::path::PathBuf::from("/tmp/plans/x.md"));
        app.plan_mode.plan_form.on_plan_ready();
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(text.contains("Plan complete"), "form title missing");
        assert!(text.contains("Implement plan"), "menu missing");
        assert!(text.contains("Refine plan"));
        assert!(!text.contains("Message Craft…"), "input hidden under form");
        assert!(text.contains("tab to switch"), "status bar kept");
        assert_eq!(
            app.view.paint.lock().unwrap().composer_area,
            crate::tui::geom::Rect::default(),
            "no composer selection region while the form is up"
        );

        app.plan_mode.plan_form.hide();
        screen.draw_settled(&mut app);
        let text = screen.text();
        assert!(!text.contains("Plan complete"));
        assert!(text.contains("Message Craft…"), "input back");
        assert!(text.contains("Plan Ctrl+T"), "dismissed hint missing");
    }
}
