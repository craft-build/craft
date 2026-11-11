pub const STUBS: &str = include_str!("stubs.pyi");
pub const DESCRIPTION_TEMPLATE: &str = include_str!("tool_description.md");

pub const TOOL_NAMES: &[&str] = &[
    "read_text",
    "read",
    "glob",
    "grep",
    "list_dir",
    "bash",
    "edit",
];

pub fn description() -> String {
    DESCRIPTION_TEMPLATE.replace("{{STUBS}}", STUBS)
}

#[test]
fn description_contains_the_type_checked_stubs() {
    let description = description();
    assert!(description.contains(STUBS));
    assert!(description.contains("await every call"));
    assert!(description.contains("Not callable here:"));
}
