//! First-run screen: craft binary discovery, working directory, SSH
//! transport, default mode, and permission toggles. Ported from the old
//! React `Onboarding`, styled with craft-code tokens.

use dioxus::prelude::*;

use crate::backend::SshTarget;
use crate::components::composer::PERMS;
use crate::state::{AppState, DEFAULT_MODES, start_from_onboarding};

#[component]
pub fn Onboarding() -> Element {
    let s = use_context::<AppState>();
    let backend = crate::backend::get();
    let mut cwd = use_signal(|| {
        craft_storage::paths::home()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let mut transport = use_signal(|| "local".to_string());
    let mut host = use_signal(String::new);
    let mut remote_cwd = use_signal(String::new);
    let mut remote_craft = use_signal(String::new);
    let mut mode = use_signal(|| "build".to_string());
    let mut perm = use_signal(|| 0usize);
    let mut perm_open = use_signal(|| false);

    let perm_index = *perm.read();
    let selected = PERMS[perm_index];

    let binary = backend.craft_binary();
    let binary_found = binary.is_file() || binary == std::path::Path::new("craft");
    let binary_display = format!("craft found at {}", binary.display());
    let starting = *s.starting.read();
    let error = s.start_error.read().clone();

    let start = move || {
        if *transport.read() == "local" {
            if cwd.read().is_empty() {
                return;
            }
            start_from_onboarding(
                s,
                backend,
                cwd.read().clone(),
                selected.yolo,
                None,
                (*mode.read() != "build").then(|| mode.cloned()),
                selected.auto_review,
            );
        } else {
            let host_val = host.read().trim().to_string();
            let remote_val = remote_cwd.read().trim().to_string();
            if host_val.is_empty() || remote_val.is_empty() || !remote_val.starts_with('/') {
                return;
            }
            let craft = remote_craft.read().trim().to_string();
            start_from_onboarding(
                s,
                backend,
                remote_val,
                selected.yolo,
                Some(SshTarget {
                    host: host_val,
                    remote_craft: (!craft.is_empty()).then_some(craft),
                }),
                (*mode.read() != "build").then(|| mode.cloned()),
                selected.auto_review,
            );
        }
    };

    rsx! {
        div { class: "onboarding",
            div { class: "ob-card",
                div { class: "ob-brand",
                    div { class: "ob-logo", "craft" }
                    div {
                        div { class: "ob-title", "Craft Desktop" }
                        div { class: "ob-subtitle", "AGENTIC WORKSPACE ON THE CRAFT HARNESS" }
                    }
                }

                div { class: if binary_found { "ob-binary ob-binary-ok" } else { "ob-binary ob-binary-bad" },
                    if binary_found {
                        "{binary_display}"
                    } else {
                        "craft binary not found — install craft, or set CRAFT_DESKTOP_BINARY"
                    }
                }

                div { class: "ob-section",
                    div { class: "ob-label", "TRANSPORT" }
                    div { class: "ob-seg",
                        button {
                            class: if *transport.read() == "local" { "ob-seg-item active" } else { "ob-seg-item" },
                            onclick: move |_| transport.set("local".to_string()),
                            "Local"
                        }
                        button {
                            class: if *transport.read() == "ssh" { "ob-seg-item active" } else { "ob-seg-item" },
                            onclick: move |_| transport.set("ssh".to_string()),
                            "SSH"
                        }
                    }
                }

                if *transport.read() == "local" {
                    div { class: "ob-section",
                        div { class: "ob-label", "WORKING DIRECTORY" }
                        div { class: "ob-field",
                            input {
                                class: "ob-input grow",
                                value: "{cwd}",
                                oninput: move |e| cwd.set(e.value()),
                            }
                            button { class: "btn btn-secondary btn-sm", onclick: move |_| {
                                    let dir = cwd.read().clone();
                                    if let Some(picked) = rfd::FileDialog::new()
                                        .set_directory(dir)
                                        .pick_folder()
                                    {
                                        cwd.set(picked.to_string_lossy().into_owned());
                                    }
                                },
                                "Browse"
                            }
                        }
                    }
                } else {
                    div { class: "ob-section",
                        div { class: "ob-label", "HOST" }
                        input {
                            class: "ob-input",
                            placeholder: "user@host",
                            value: "{host}",
                            oninput: move |e| host.set(e.value()),
                        }
                        div { class: "ob-label", "REMOTE PATH" }
                        input {
                            class: "ob-input",
                            placeholder: "/home/user/project",
                            value: "{remote_cwd}",
                            oninput: move |e| remote_cwd.set(e.value()),
                        }
                        div { class: "ob-label", "REMOTE CRAFT PATH (OPTIONAL)" }
                        input {
                            class: "ob-input",
                            placeholder: "craft",
                            value: "{remote_craft}",
                            oninput: move |e| remote_craft.set(e.value()),
                        }
                        div { class: "ob-hint",
                            "Leave blank to use craft on the remote PATH. Requires key-based auth and an accepted host key."
                        }
                    }
                }

                div { class: "ob-section",
                    div { class: "ob-label", "DEFAULT MODE" }
                    div { class: "ob-seg",
                        for m in DEFAULT_MODES {
                            button {
                                class: if *mode.read() == m { "ob-seg-item active" } else { "ob-seg-item" },
                                key: "{m}",
                                onclick: move |_| mode.set(m.to_string()),
                                "{crate::state::mode_label(m)}"
                            }
                        }
                    }
                }

                div { class: "ob-section",
                    div { class: "ob-label", "PERMISSIONS" }
                    div { class: "dd",
                        button { class: "bar-btn perm", onclick: move |_| perm_open.toggle(),
                            "{selected.label}"
                            span { class: "caret", "▾" }
                        }
                        if *perm_open.read() {
                            div { class: "menu perm",
                                for (i, p) in PERMS.iter().enumerate() {
                                    button {
                                        class: if i == perm_index { "menu-item-col selected" } else { "menu-item-col" },
                                        key: "{i}",
                                        onclick: move |_| {
                                            perm.set(i);
                                            perm_open.set(false);
                                        },
                                        span { class: "menu-check", if i == perm_index { "✓" } else { "" } }
                                        span { class: "menu-item-body",
                                            span { class: "menu-item-title", "{p.label}" }
                                            span { class: "menu-item-desc", "{p.desc}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                button {
                    class: if starting { "ob-start disabled" } else { "ob-start" },
                    disabled: starting,
                    onclick: move |_| start(),
                    if starting { "Starting…" } else { "Start session →" }
                }
                if let Some(err) = error {
                    div { class: "start-error", "{err}" }
                }
            }
        }
    }
}
