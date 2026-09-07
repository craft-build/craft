//! Data model transliterated from the Forge design's `Component` class
//! (state shape, seed data, canned assistant replies).

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Onboarding,
    Projects,
    Workspace,
    Settings,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub desc: String,
    pub updated: String,
    pub checkpoint_label: String,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug)]
pub enum DiffLineKind {
    Ctx,
    Add,
    Del,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Diff {
    pub file: String,
    pub stat: String,
    pub hunk_header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
pub struct Terminal {
    pub cmd: String,
    pub output: String,
}

#[derive(Clone, Debug)]
pub struct Steps {
    pub summary: String,
    pub items: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub author: String,
    pub text: String,
    pub pending: bool,
    pub label: String,
}

#[derive(Clone, Debug)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub time: Option<String>,
    pub context: Vec<String>,
    pub attached_comments: Vec<(String, String)>,
    pub checkpoint_label: Option<String>,
    pub steps: Option<Steps>,
    pub diff: Option<Diff>,
    pub terminal: Option<Terminal>,
}

impl Message {
    pub fn user(id: &str, text: &str) -> Self {
        Message {
            id: id.into(),
            role: Role::User,
            text: text.into(),
            time: None,
            context: vec![],
            attached_comments: vec![],
            checkpoint_label: None,
            steps: None,
            diff: None,
            terminal: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: &'static str,
    pub status: Option<&'static str>,
}

pub const CHANGED_FILES: &[FileEntry] = &[
    FileEntry {
        path: "src/lib/cart/totals.ts",
        status: Some("modified"),
    },
    FileEntry {
        path: "src/lib/cart/totals.test.ts",
        status: Some("added"),
    },
];

pub fn seed_file_diffs() -> HashMap<String, Diff> {
    let mut m = HashMap::new();
    m.insert(
        "src/lib/cart/totals.ts".to_string(),
        Diff {
            file: "src/lib/cart/totals.ts".into(),
            stat: "+6 -1".into(),
            hunk_header: "@@ -18,2 +18,7 @@".into(),
            lines: vec![
                DiffLine { kind: DiffLineKind::Ctx, text: "export function recomputeTotals(cart: Cart): Totals {".into() },
                DiffLine { kind: DiffLineKind::Del, text: "  return calculate(cart.items, cart.discounts);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  const key = signature(cart.items, cart.discounts);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  if (cache.key === key) return cache.value;".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  const value = calculate(cart.items, cart.discounts);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  cache = { key, value };".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  return value;".into() },
                DiffLine { kind: DiffLineKind::Ctx, text: "}".into() },
            ],
        },
    );
    m.insert(
        "src/lib/cart/totals.test.ts".to_string(),
        Diff {
            file: "src/lib/cart/totals.test.ts".into(),
            stat: "+9 -0".into(),
            hunk_header: "@@ -0,0 +1,9 @@".into(),
            lines: vec![
                DiffLine { kind: DiffLineKind::Add, text: "describe('recomputeTotals cache', () => {".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  it('invalidates when a new discount is applied mid-session', () => {".into() },
                DiffLine { kind: DiffLineKind::Add, text: "    const cart = buildCart({ items: TWO_ITEMS });".into() },
                DiffLine { kind: DiffLineKind::Add, text: "    recomputeTotals(cart);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "    applyDiscount(cart, PERCENT_10);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "    const totals = recomputeTotals(cart);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "    expect(totals.discountTotal).toBeGreaterThan(0);".into() },
                DiffLine { kind: DiffLineKind::Add, text: "  });".into() },
                DiffLine { kind: DiffLineKind::Add, text: "});".into() },
            ],
        },
    );
    m
}

pub struct Canned {
    pub text: &'static str,
    pub steps: Option<Steps>,
    pub diff: Option<Diff>,
    pub terminal: Option<Terminal>,
}

pub fn canned_replies() -> Vec<Canned> {
    vec![
        Canned {
            text: "Looked at src/routes/checkout.tsx — it's already wired to the memoized path, so this should just work. Want a loading state while the cache warms on first render?",
            steps: Some(Steps {
                summary: "Thought 1 time · read 1 file".into(),
                items: vec!["Read src/routes/checkout.tsx".into()],
            }),
            diff: None,
            terminal: None,
        },
        Canned {
            text: "Done — added a guard so an empty cart short-circuits before touching the cache at all.",
            steps: Some(Steps {
                summary: "Thought 1 time · wrote 1 file · ran 1 test".into(),
                items: vec![
                    "Edited src/lib/cart/totals.ts".into(),
                    "Ran pnpm test cart".into(),
                ],
            }),
            diff: Some(Diff {
                file: "src/lib/cart/totals.ts".into(),
                stat: "+3 -0".into(),
                hunk_header: "@@ -18,1 +18,4 @@".into(),
                lines: vec![
                    DiffLine { kind: DiffLineKind::Ctx, text: "export function recomputeTotals(cart: Cart): Totals {".into() },
                    DiffLine { kind: DiffLineKind::Add, text: "  if (cart.items.length === 0) return EMPTY_TOTALS;".into() },
                    DiffLine { kind: DiffLineKind::Ctx, text: "  const key = signature(cart.items, cart.discounts);".into() },
                ],
            }),
            terminal: Some(Terminal {
                cmd: "pnpm test cart".into(),
                output: "✓ 36 passed  0 failed  (401ms)".into(),
            }),
        },
    ]
}

pub fn seed_projects() -> Vec<Project> {
    vec![
        Project {
            id: "orbitkit".into(),
            name: "orbitkit".into(),
            path: "~/dev/orbitkit".into(),
            desc: "Cart totals cache + regression tests".into(),
            updated: "2h ago".into(),
            checkpoint_label: "2 checkpoints".into(),
            model: "Sable Large".into(),
        },
        Project {
            id: "weather-cli".into(),
            name: "weather-cli".into(),
            path: "~/dev/weather-cli".into(),
            desc: "Retry logic for a flaky NOAA endpoint".into(),
            updated: "Yesterday".into(),
            checkpoint_label: "5 checkpoints".into(),
            model: "Sable Fast".into(),
        },
        Project {
            id: "notes-sync".into(),
            name: "notes-sync".into(),
            path: "~/dev/notes-sync".into(),
            desc: "Conflict resolution for offline edits".into(),
            updated: "3 days ago".into(),
            checkpoint_label: "8 checkpoints".into(),
            model: "Local 8B".into(),
        },
    ]
}

pub fn seed_sessions() -> HashMap<String, Vec<Session>> {
    let mut m = HashMap::new();

    let m2_diff = Diff {
        file: "src/lib/cart/totals.ts".into(),
        stat: "+6 -1".into(),
        hunk_header: "@@ -18,2 +18,7 @@".into(),
        lines: vec![
            DiffLine { kind: DiffLineKind::Ctx, text: "export function recomputeTotals(cart: Cart): Totals {".into() },
            DiffLine { kind: DiffLineKind::Del, text: "  return calculate(cart.items, cart.discounts);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  const key = signature(cart.items, cart.discounts);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  if (cache.key === key) return cache.value;".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  const value = calculate(cart.items, cart.discounts);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  cache = { key, value };".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  return value;".into() },
            DiffLine { kind: DiffLineKind::Ctx, text: "}".into() },
        ],
    };
    let m4_diff = Diff {
        file: "src/lib/cart/totals.test.ts".into(),
        stat: "+9 -0".into(),
        hunk_header: "@@ -0,0 +1,9 @@".into(),
        lines: vec![
            DiffLine { kind: DiffLineKind::Add, text: "describe('recomputeTotals cache', () => {".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  it('invalidates when a new discount is applied mid-session', () => {".into() },
            DiffLine { kind: DiffLineKind::Add, text: "    const cart = buildCart({ items: TWO_ITEMS });".into() },
            DiffLine { kind: DiffLineKind::Add, text: "    recomputeTotals(cart);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "    applyDiscount(cart, PERCENT_10);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "    const totals = recomputeTotals(cart);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "    expect(totals.discountTotal).toBeGreaterThan(0);".into() },
            DiffLine { kind: DiffLineKind::Add, text: "  });".into() },
            DiffLine { kind: DiffLineKind::Add, text: "});".into() },
        ],
    };

    m.insert(
        "orbitkit".to_string(),
        vec![Session {
            id: "s1".into(),
            name: "Cart totals cache".into(),
            messages: vec![
                Message::user(
                    "m1",
                    "The checkout flow is stalling under load — can you find where cart totals are recomputed and cache it if it's cheap to do safely?",
                ),
                Message {
                    id: "m2".into(),
                    role: Role::Assistant,
                    text: "Done — recomputeTotals now memoizes on the cart's item signature and only recalculates when items, quantities, or discounts change. Everything else reads from cache.".into(),
                    time: Some("14m ago".into()),
                    context: vec![],
                    attached_comments: vec![],
                    checkpoint_label: Some("Checkpoint 1 · Memoize recomputeTotals".into()),
                    steps: Some(Steps {
                        summary: "Thought 2 times · ran 1 search · read 2 files".into(),
                        items: vec![
                            "Searched \"recomputeTotals\" in src/**".into(),
                            "Read src/lib/cart/totals.ts".into(),
                            "Read src/lib/cart/store.ts".into(),
                        ],
                    }),
                    diff: Some(m2_diff),
                    terminal: Some(Terminal {
                        cmd: "pnpm test cart".into(),
                        output: "✓ 34 passed  0 failed  (412ms)".into(),
                    }),
                },
                Message::user("m3", "Nice. Add a unit test for the discount stacking edge case before we ship this."),
                Message {
                    id: "m4".into(),
                    role: Role::Assistant,
                    text: "Added a case for two stacked percentage discounts plus a flat discount — confirms the cache invalidates correctly when a new discount is applied mid-session.".into(),
                    time: Some("6m ago".into()),
                    context: vec![],
                    attached_comments: vec![],
                    checkpoint_label: Some("Checkpoint 2 · Add discount-stacking test".into()),
                    steps: Some(Steps {
                        summary: "Thought 1 time · wrote 1 file · ran 1 test".into(),
                        items: vec![
                            "Wrote src/lib/cart/totals.test.ts".into(),
                            "Ran pnpm test cart".into(),
                        ],
                    }),
                    diff: Some(m4_diff),
                    terminal: Some(Terminal {
                        cmd: "pnpm test cart".into(),
                        output: "✓ 35 passed  0 failed  (438ms)".into(),
                    }),
                },
            ],
        }],
    );
    m.insert(
        "weather-cli".to_string(),
        vec![Session { id: "s1".into(), name: "Retry logic".into(), messages: vec![] }],
    );
    m.insert(
        "notes-sync".to_string(),
        vec![Session { id: "s1".into(), name: "Conflict resolution".into(), messages: vec![] }],
    );
    m
}

pub fn seed_comments() -> HashMap<String, Vec<Comment>> {
    let mut m = HashMap::new();
    m.insert(
        "m2_3".to_string(),
        vec![Comment {
            author: "you".into(),
            text: "what happens if the discounts array gets mutated in place elsewhere?".into(),
            pending: false,
            label: "comment".into(),
        }],
    );
    m
}

pub const MODEL_NAMES: &[&str] = &["Sable Large", "Sable Fast", "Local 8B"];
