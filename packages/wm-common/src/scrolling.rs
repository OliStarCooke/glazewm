use std::str::FromStr;

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// When to center the focused column in a scrolling workspace.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CenterFocusedColumn {
  /// No special centering; focus scrolls column to nearest edge.
  #[default]
  Never,

  /// Focused column is always centered.
  Always,

  /// Focused column is centered when it doesn't fit on screen together
  /// with the previously focused column.
  #[serde(alias = "on-overflow")]
  OnOverflow,
}

impl FromStr for CenterFocusedColumn {
  type Err = anyhow::Error;

  /// Parses a string into a centering mode.
  fn from_str(unparsed: &str) -> anyhow::Result<Self> {
    match unparsed {
      "never" => Ok(Self::Never),
      "always" => Ok(Self::Always),
      "on-overflow" | "on_overflow" => Ok(Self::OnOverflow),
      _ => bail!("Not a valid center-focused-column mode: {}", unparsed),
    }
  }
}

/// Default width of a new column as a fraction of viewport width.
fn default_column_width() -> f32 {
  0.5
}

/// Default preset widths cycled by `switch-preset-column-width`.
fn default_preset_column_widths() -> Vec<f32> {
  vec![1.0 / 3.0, 0.5, 2.0 / 3.0]
}

const fn default_animation_duration_ms() -> u64 {
  250
}

const fn default_true() -> bool {
  true
}

/// Scrolling layout options (niri-style).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all(serialize = "camelCase"))]
pub struct ScrollingConfig {
  /// When to center the focused column.
  pub center_focused_column: CenterFocusedColumn,

  /// Whether a lone column is always centered regardless of
  /// `center_focused_column`.
  pub always_center_single_column: bool,

  /// Width of new columns as a fraction of the viewport width.
  #[serde(default = "default_column_width")]
  pub default_column_width: f32,

  /// Widths cycled by `switch-preset-column-width`.
  #[serde(default = "default_preset_column_widths")]
  pub preset_column_widths: Vec<f32>,

  /// Whether scroll offset changes are animated.
  #[serde(default = "default_true")]
  pub animation_enabled: bool,

  /// Duration of scroll animations in milliseconds.
  #[serde(default = "default_animation_duration_ms")]
  pub animation_duration_ms: u64,
}

impl Default for ScrollingConfig {
  fn default() -> Self {
    Self {
      center_focused_column: CenterFocusedColumn::Never,
      always_center_single_column: false,
      default_column_width: default_column_width(),
      preset_column_widths: default_preset_column_widths(),
      animation_enabled: true,
      animation_duration_ms: default_animation_duration_ms(),
    }
  }
}

impl ScrollingConfig {
  /// Default column width clamped to a sane range.
  pub fn normalized_default_width(&self) -> f32 {
    self.default_column_width.clamp(0.05, 1.0)
  }

  /// Preset widths clamped to a sane range with invalid entries removed.
  pub fn normalized_presets(&self) -> Vec<f32> {
    let presets = self
      .preset_column_widths
      .iter()
      .map(|width| width.clamp(0.05, 1.0))
      .collect::<Vec<_>>();

    if presets.is_empty() {
      default_preset_column_widths()
    } else {
      presets
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_centering_mode() {
    assert_eq!(
      CenterFocusedColumn::from_str("never").unwrap(),
      CenterFocusedColumn::Never
    );
    assert_eq!(
      CenterFocusedColumn::from_str("always").unwrap(),
      CenterFocusedColumn::Always
    );
    assert_eq!(
      CenterFocusedColumn::from_str("on-overflow").unwrap(),
      CenterFocusedColumn::OnOverflow
    );
    assert!(CenterFocusedColumn::from_str("sometimes").is_err());
  }

  #[test]
  fn defaults_match_niri_like_behavior() {
    let config = ScrollingConfig::default();
    assert_eq!(config.center_focused_column, CenterFocusedColumn::Never);
    assert!(!config.always_center_single_column);
    assert_eq!(config.normalized_default_width(), 0.5);
    assert_eq!(config.normalized_presets().len(), 3);
    assert!(config.animation_enabled);
  }

  #[test]
  fn normalizes_out_of_range_widths() {
    let config = ScrollingConfig {
      default_column_width: 5.0,
      preset_column_widths: vec![0.0, 0.5, 2.0],
      ..Default::default()
    };

    assert_eq!(config.normalized_default_width(), 1.0);
    assert_eq!(config.normalized_presets(), vec![0.05, 0.5, 1.0]);
  }
}
