//! UI components for the craft desktop app.

pub mod composer;
pub mod diff;
pub mod icons;
pub mod new_task;
pub mod onboarding;
pub mod palette;
pub mod panel;
pub mod session;
pub mod sidebar;
pub mod skills;
pub mod topbar;
pub mod transcript;
pub mod ui;

pub use crate::markdown::Markdown;
pub use composer::Composer;
pub use diff::DiffLines;
pub use icons::{
    IconAutomations, IconBranch, IconChanges, IconClose, IconFolder, IconHelp, IconImage, IconList,
    IconLogo, IconNew, IconPanel, IconRepo, IconSearch, IconSend, IconShield, IconSkills, IconStop,
};
pub use new_task::NewTaskView;
pub use onboarding::Onboarding;
pub use palette::Palette;
pub use panel::ChangesPanel;
pub use session::SessionView;
pub use sidebar::Sidebar;
pub use skills::SkillsView;
pub use topbar::TopBar;
pub use transcript::{PlanCard, TodoList, Transcript};
pub use ui::{PillState, StatusPill};
