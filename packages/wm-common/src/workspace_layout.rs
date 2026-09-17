use std::str::FromStr;

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// Layout of a `Workspace`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLayout {
  /// Classic tiling where siblings share the parent's rect
  /// proportionally.
  #[default]
  Tiling,

  /// Niri-style scrolling where top-level columns keep stable widths
  /// on an infinite horizontal strip and the viewport scrolls to
  /// follow focus.
  Scrolling,
}

impl FromStr for WorkspaceLayout {
  type Err = anyhow::Error;

  /// Parses a string into a workspace layout.
  fn from_str(unparsed: &str) -> anyhow::Result<Self> {
    match unparsed {
      "tiling" => Ok(Self::Tiling),
      "scrolling" => Ok(Self::Scrolling),
      _ => bail!("Not a valid workspace layout: {}", unparsed),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_layout() {
    assert_eq!(
      WorkspaceLayout::from_str("tiling").unwrap(),
      WorkspaceLayout::Tiling
    );
    assert_eq!(
      WorkspaceLayout::from_str("scrolling").unwrap(),
      WorkspaceLayout::Scrolling
    );
    assert!(WorkspaceLayout::from_str("grid").is_err());
  }

  #[test]
  fn defaults_to_tiling() {
    assert_eq!(WorkspaceLayout::default(), WorkspaceLayout::Tiling);
  }
}
