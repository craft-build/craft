//! The Skills section: browse project and global skills, add/edit/delete
//! them on disk, and spawn an agent session that drafts a skill for you.

use dioxus::prelude::*;

use crate::skills::{Skill, SkillScope};
use crate::state::{
    AppState, begin_edit_skill, begin_new_skill, cancel_skill_editor, create_skill_with_ai,
    delete_skill, refresh_skills, save_skill_editor, skills_cwd,
};

#[component]
pub fn SkillsView() -> Element {
    let mut s = use_context::<AppState>();
    let backend = crate::backend::get();

    let mut ai_desc = use_signal(String::new);
    let mut ai_scope = use_signal(|| SkillScope::Project);

    let skills = s.skills.read().clone();
    let error = s.skills_error.read().clone();
    let delete_target = s.skill_delete.read().clone();
    let ai_open = *s.skill_ai_open.read();
    let cwd = skills_cwd(s).unwrap_or_default();

    if let Some(draft) = s.skill_editor.read().clone() {
        let editing = draft.target.is_some();
        return rsx! {
            div { class: "skills-view",
                div { class: "skills-head",
                    span { class: "skills-title", if editing { "Edit skill" } else { "New skill" } }
                }
                if let Some(e) = error {
                    div { class: "skills-error", "{e}" }
                }
                div { class: "skill-editor",
                    if !editing {
                        div { class: "skills-field",
                            span { class: "skills-label", "Location" }
                            div { class: "skills-scope",
                                label {
                                    input {
                                        r#type: "radio",
                                        name: "skill-scope",
                                        checked: draft.scope == SkillScope::Project,
                                        onchange: move |_| s.skill_editor.with_mut(|d| {
                                            if let Some(d) = d.as_mut() { d.scope = SkillScope::Project; }
                                        }),
                                    }
                                    span { "Project (.craft/skills)" }
                                }
                                label {
                                    input {
                                        r#type: "radio",
                                        name: "skill-scope",
                                        checked: draft.scope == SkillScope::Global,
                                        onchange: move |_| s.skill_editor.with_mut(|d| {
                                            if let Some(d) = d.as_mut() { d.scope = SkillScope::Global; }
                                        }),
                                    }
                                    span { "Global (all projects)" }
                                }
                            }
                        }
                    }
                    div { class: "skills-field",
                        span { class: "skills-label", "Name" }
                        input {
                            class: "skills-input",
                            placeholder: "kebab-case-name",
                            value: "{draft.name}",
                            disabled: editing,
                            oninput: move |e| s.skill_editor.with_mut(|d| {
                                if let Some(d) = d.as_mut() { d.name = e.value(); }
                            }),
                        }
                    }
                    div { class: "skills-field",
                        span { class: "skills-label", "Description" }
                        input {
                            class: "skills-input",
                            placeholder: "What the skill does and when to load it",
                            value: "{draft.description}",
                            oninput: move |e| s.skill_editor.with_mut(|d| {
                                if let Some(d) = d.as_mut() { d.description = e.value(); }
                            }),
                        }
                    }
                    div { class: "skills-field",
                        span { class: "skills-label", "When to use" }
                        input {
                            class: "skills-input",
                            placeholder: "Optional: trigger conditions for the agent",
                            value: "{draft.when_to_use}",
                            oninput: move |e| s.skill_editor.with_mut(|d| {
                                if let Some(d) = d.as_mut() { d.when_to_use = e.value(); }
                            }),
                        }
                    }
                    div { class: "skills-field",
                        span { class: "skills-label", "Body" }
                        textarea {
                            class: "skills-textarea",
                            rows: "12",
                            placeholder: "Step-by-step instructions a future agent could follow",
                            value: "{draft.body}",
                            oninput: move |e| s.skill_editor.with_mut(|d| {
                                if let Some(d) = d.as_mut() { d.body = e.value(); }
                            }),
                        }
                    }
                    div { class: "skills-actions",
                        button {
                            class: "btn btn-md btn-primary",
                            onclick: move |_| save_skill_editor(s),
                            if editing { "Save" } else { "Create skill" }
                        }
                        button {
                            class: "btn btn-md btn-secondary",
                            onclick: move |_| cancel_skill_editor(s),
                            "Cancel"
                        }
                    }
                }
            }
        };
    }

    let project: Vec<Skill> = skills
        .iter()
        .filter(|k| k.scope == SkillScope::Project)
        .cloned()
        .collect();
    let global: Vec<Skill> = skills
        .iter()
        .filter(|k| k.scope == SkillScope::Global)
        .cloned()
        .collect();

    rsx! {
        div { class: "skills-view",
            div { class: "skills-head",
                div {
                    span { class: "skills-title", "Skills" }
                    span { class: "skills-subtitle", "project: {cwd}" }
                }
                div { class: "grow" }
                button {
                    class: "btn btn-md btn-secondary",
                    onclick: move |_| refresh_skills(s),
                    "Refresh"
                }
                button {
                    class: "btn btn-md btn-secondary",
                    onclick: move |_| begin_new_skill(s),
                    "New skill"
                }
                button {
                    class: "btn btn-md btn-primary",
                    onclick: move |_| s.skill_ai_open.toggle(),
                    "Create with AI"
                }
            }
            if let Some(e) = error {
                div { class: "skills-error", "{e}" }
            }
            if ai_open {
                div { class: "skills-ai",
                    span { class: "skills-label", "Describe the skill you want" }
                    textarea {
                        class: "skills-textarea",
                        rows: "4",
                        placeholder: "e.g. A workflow that reviews a PR and posts a summary",
                        value: "{ai_desc}",
                        oninput: move |e| ai_desc.set(e.value()),
                    }
                    div { class: "skills-scope",
                        label {
                            input {
                                r#type: "radio",
                                name: "ai-scope",
                                checked: *ai_scope.read() == SkillScope::Project,
                                onchange: move |_| ai_scope.set(SkillScope::Project),
                            }
                            span { "Project" }
                        }
                        label {
                            input {
                                r#type: "radio",
                                name: "ai-scope",
                                checked: *ai_scope.read() == SkillScope::Global,
                                onchange: move |_| ai_scope.set(SkillScope::Global),
                            }
                            span { "Global" }
                        }
                        div { class: "grow" }
                        button {
                            class: "btn btn-md btn-primary",
                            onclick: move |_| {
                                create_skill_with_ai(s, backend, ai_desc.read().clone(), *ai_scope.read());
                                ai_desc.set(String::new());
                            },
                            "Launch agent"
                        }
                        button {
                            class: "btn btn-md btn-secondary",
                            onclick: move |_| {
                                s.skill_ai_open.set(false);
                                ai_desc.set(String::new());
                            },
                            "Cancel"
                        }
                    }
                }
            }
            if project.is_empty() && global.is_empty() {
                div { class: "skills-empty", "No skills yet. Create one manually or with AI." }
            }
            SkillGroup { title: "Project", skills: project, delete_target: delete_target.clone() }
            SkillGroup { title: "Global", skills: global, delete_target }
        }
    }
}

#[component]
fn SkillGroup(title: &'static str, skills: Vec<Skill>, delete_target: Option<Skill>) -> Element {
    let mut s = use_context::<AppState>();
    if skills.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "skills-group-label", "{title}" }
        for skill in skills {
            {
                let pending_delete = delete_target.as_ref().is_some_and(|t| t.path == skill.path);
                let row_path = skill.path.display().to_string();
                let row_skill = skill.clone();
                let row_skill2 = skill.clone();
                rsx! {
                    div { class: "skill-row", key: "{skill.path:?}",
                        div { class: "skill-main",
                            span { class: "skill-name", "{skill.name}" }
                            span { class: "skill-desc",
                                if skill.description.is_empty() { "No description" } else { "{skill.description}" }
                            }
                            span { class: "skill-path", "{row_path}" }
                        }
                        if pending_delete {
                            div { class: "skill-confirm",
                                span { "Delete this skill?" }
                                button {
                                    class: "btn btn-md btn-primary",
                                    onclick: move |_| delete_skill(s, skill.clone()),
                                    "Delete"
                                }
                                button {
                                    class: "btn btn-md btn-secondary",
                                    onclick: move |_| s.skill_delete.set(None),
                                    "Keep"
                                }
                            }
                        } else {
                            div { class: "skill-actions",
                                button {
                                    class: "btn btn-md btn-secondary",
                                    onclick: move |_| begin_edit_skill(s, row_skill.clone()),
                                    "Edit"
                                }
                                button {
                                    class: "btn btn-md btn-secondary",
                                    onclick: move |_| s.skill_delete.set(Some(row_skill2.clone())),
                                    "Delete"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
