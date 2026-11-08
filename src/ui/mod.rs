//! TUI application.

pub mod highlight;
pub mod markdown;
pub mod theme;

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
pub async fn run(_runtime: Runtime, _options: UiOptions) -> Result<()> {
    anyhow::bail!("the TUI is not implemented yet")
}
