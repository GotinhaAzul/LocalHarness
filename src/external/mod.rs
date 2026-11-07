//! External tool manifests and the stdin/stdout protocol.

use std::sync::Arc;

use anyhow::Result;

use crate::paths::Paths;
use crate::tools::Tool;

/// Discover trusted external tools. Project manifests require trust.
pub fn load_tools(_paths: &Paths, _trust: bool) -> Result<Vec<Arc<dyn Tool>>> {
    Ok(Vec::new())
}
