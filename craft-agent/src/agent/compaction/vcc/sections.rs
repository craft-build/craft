use super::brief::{build_brief_sections, identify_turns, stringify_brief};
use super::extract::commits::{extract_commits, format_commits};
use super::extract::files_symbols::{
    extract_file_and_symbol_data, format_file_activity, format_type_catalog,
};
use super::extract::goals::extract_goals;
use super::extract::outstanding::extract_outstanding_context;
use super::extract::preferences::{dedup_preferences_against_goals, extract_preferences};
use super::format::SectionData;
use super::normalize::NormalizedBlock;

/// Build all summary sections from filtered, normalized blocks.
pub(crate) fn build_sections(blocks: &[NormalizedBlock]) -> SectionData {
    let file_and_symbols = extract_file_and_symbol_data(blocks);
    let session_goal = extract_goals(blocks);
    let user_preferences =
        dedup_preferences_against_goals(extract_preferences(blocks), &session_goal);
    let turn_summaries: Vec<String> = identify_turns(blocks)
        .into_iter()
        .map(|t| t.summary)
        .collect();
    let outstanding_context = extract_outstanding_context(blocks);
    let commits = format_commits(&extract_commits(blocks), 8);
    let files_and_changes = format_file_activity(&file_and_symbols);
    let type_catalog = format_type_catalog(&file_and_symbols);
    let brief_transcript = stringify_brief(&build_brief_sections(blocks));

    SectionData {
        session_goal,
        user_preferences,
        files_and_changes,
        commits,
        type_catalog,
        outstanding_context,
        turn_summaries,
        brief_transcript,
    }
}
