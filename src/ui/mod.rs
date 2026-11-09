//! TUI application.

pub mod complete;
pub mod highlight;
pub mod input;
pub mod markdown;
pub mod render;
pub mod theme;
pub mod tree;
pub mod tui;
pub mod widgets;

use anyhow::Result;

use crate::app::Runtime;
use crate::skills::SkillIndex;

#[derive(Debug, Clone)]
pub struct UiOptions {
    pub no_color: bool,
    pub trust: bool,
    pub skills: SkillIndex,
}

/// Run the interactive TUI until the user quits.
pub async fn run(runtime: Runtime, options: UiOptions) -> Result<()> {
    tui::run(runtime, options).await
}
