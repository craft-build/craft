use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::ToolOutput;
use crate::styleguide;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct StyleguideList {
    #[param(description = "Language to list styleguides for (e.g., 'rust', 'general')")]
    language: String,
}

impl StyleguideList {
    pub const NAME: &str = "styleguide_list";
    pub const DESCRIPTION: &str = "List available styleguide categories for a language. Use this to discover what styleguides are available before fetching specific rules.";
    pub const EXAMPLES: Option<&str> = Some(r#"[{"language": "rust"}]"#);

    pub fn start_header(&self) -> String {
        format!("styleguide_list({})", self.language)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::Plain(styleguide::list_categories(
            &self.language,
        )))
    }
}

super::impl_tool!(StyleguideList, kind = "search");

impl super::ToolInvocation for StyleguideList {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(StyleguideList::start_header(
            self,
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { StyleguideList::execute(&self, ctx).await })
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct StyleguideSearch {
    #[param(description = "Search query — rule ID, keyword, or phrase")]
    query: String,
    #[param(description = "Filter by language (e.g., 'rust'). Omit to search all.")]
    language: Option<String>,
    #[param(description = "Filter by category (e.g., 'naming'). Omit to search all.")]
    category: Option<String>,
    #[param(description = "Filter by tags.")]
    tags: Option<Vec<String>>,
    #[param(description = "Maximum results (default: 10)")]
    limit: Option<usize>,
}

impl StyleguideSearch {
    pub const NAME: &str = "styleguide_search";
    pub const DESCRIPTION: &str = "Search for styleguide rules by keywords, rule IDs, or tags. Returns matching rules sorted by relevance.";
    pub const EXAMPLES: Option<&str> = Some(r#"[{"query": "naming", "language": "rust"}]"#);

    pub fn start_header(&self) -> String {
        format!("styleguide_search({})", self.query)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::Plain(styleguide::search_rules(
            &self.query,
            self.language.as_deref(),
            self.category.as_deref(),
            self.tags.as_ref(),
            self.limit,
        )))
    }
}

super::impl_tool!(StyleguideSearch, kind = "search");

impl super::ToolInvocation for StyleguideSearch {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(StyleguideSearch::start_header(
            self,
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { StyleguideSearch::execute(&self, ctx).await })
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct StyleguideGet {
    #[param(description = "Language code (e.g., 'rust', 'general')")]
    language: String,
    #[param(
        description = "Category to fetch (e.g., 'naming'). Required unless using rule_ids or file_path."
    )]
    category: Option<String>,
    #[param(description = "Specific rule IDs to fetch.")]
    rule_ids: Option<Vec<String>>,
    #[param(description = "File path to auto-detect language and get minimal context.")]
    file_path: Option<String>,
}

impl StyleguideGet {
    pub const NAME: &str = "styleguide_get";
    pub const DESCRIPTION: &str = "Fetch specific styleguide rules or entire categories. Can fetch by category, rule IDs, or auto-detect from file path.";
    pub const EXAMPLES: Option<&str> = Some(r#"[{"language": "rust", "category": "naming"}]"#);

    pub fn start_header(&self) -> String {
        if let Some(ref fp) = self.file_path {
            return format!("styleguide_get({fp})");
        }
        format!(
            "styleguide_get({}/{})",
            self.language,
            self.category.as_deref().unwrap_or("*")
        )
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        styleguide::get_rules(
            &self.language,
            self.category.as_deref(),
            self.rule_ids.as_ref(),
            self.file_path.as_deref(),
        )
        .map(ToolOutput::Plain)
    }
}

super::impl_tool!(StyleguideGet, kind = "search");

impl super::ToolInvocation for StyleguideGet {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(StyleguideGet::start_header(
            self,
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { StyleguideGet::execute(&self, ctx).await })
    }
}
