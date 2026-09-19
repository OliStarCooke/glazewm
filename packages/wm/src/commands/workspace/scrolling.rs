use anyhow::Context;
use wm_common::TilingDirection;
use wm_platform::Direction;

use crate::{
  commands::container::{
    move_container_within_tree, set_focused_descendant,
    wrap_in_split_container,
  },
  models::{Container, DirectionContainer, SplitContainer, TilingContainer},
  traits::{CommonGetters, TilingSizeGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Pans the viewport one column without changing focus.
///
/// Right shows later columns; left shows earlier ones. Queues a
/// redraw when the target changed.
pub fn scroll_view(
  container: &Container,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  if workspace.scroll_view(direction)? {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children())
      .queue_manual_scroll();
  }

  Ok(())
}

/// Cycles the focused column through the configured preset widths.
///
/// Queues a redraw of the workspace strip when the width changed,
/// since later columns shift horizontally.
pub fn switch_preset_column_width(
  container: &Container,
  back: bool,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  let Some(column) = workspace.focus_column(container) else {
    return Ok(());
  };

  if workspace.cycle_column_preset(&column, back) {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
  }

  Ok(())
}

/// Centers the focused column in the viewport.
pub fn center_column(
  container: &Container,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  if workspace.center_column(container)? {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
  }

  Ok(())
}

/// Toggles the focused column between full width and the default.
pub fn maximize_column(
  container: &Container,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  let Some(column) = workspace.focus_column(container) else {
    return Ok(());
  };

  if workspace.toggle_column_maximized(&column) {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
  }

  Ok(())
}

/// Moves the focused window into an adjacent column as a vertical
/// stack (niri-style consume).
///
/// Prefers the previous column; uses the next one when there is no
/// previous column. No-op when already stacked, when there is no
/// adjacent column, or outside scrolling workspaces.
pub fn consume_window_into_column(
  container: &Container,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  let Ok(window) = container.as_tiling_container() else {
    return Ok(());
  };

  let Some(column) = workspace.focus_column(container) else {
    return Ok(());
  };

  // Already stacked when the focused window sits inside a split
  // column rather than being a top-level column itself.
  if window.id() != column.id() {
    return Ok(());
  }

  let children = workspace.tiling_children().collect::<Vec<_>>();
  let Some(index) =
    children.iter().position(|child| child.id() == column.id())
  else {
    return Ok(());
  };

  let adjacent = if index > 0 {
    children.get(index - 1).cloned()
  } else {
    children.get(index + 1).cloned()
  };

  let Some(adjacent) = adjacent else {
    return Ok(());
  };

  match adjacent {
    TilingContainer::Split(adjacent_split) => {
      let target_index = adjacent_split.child_count();
      move_container_within_tree(
        &window.clone().into(),
        &adjacent_split.clone().into(),
        target_index,
        state,
      )?;
    }
    TilingContainer::TilingWindow(adjacent_window) => {
      let adjacent_width = adjacent_window.tiling_size();
      let split = SplitContainer::new(
        TilingDirection::Vertical,
        config.value.gaps.clone(),
      );

      // Preserve spatial order within the new vertical stack, with
      // the focused window joining at the bottom.
      let (first, second) = if index > 0 {
        (
          TilingContainer::TilingWindow(adjacent_window.clone()),
          window.clone(),
        )
      } else {
        (
          window.clone(),
          TilingContainer::TilingWindow(adjacent_window.clone()),
        )
      };

      wrap_in_split_container(
        &split,
        &workspace.clone().into(),
        &[first, second],
      )?;

      // Keep the column width stable instead of summing both widths.
      split.set_tiling_size(adjacent_width);
      for child in split.tiling_children() {
        child.set_tiling_size(0.5);
      }

      // Focus stays on the consumed window.
      set_focused_descendant(&window.clone().into(), None);
    }
  }

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(())
}

/// Moves the focused window out of its vertical stack into a new
/// top-level column after its current column (niri-style expel).
///
/// No-op when the window is already a lone column or outside
/// scrolling workspaces.
pub fn expel_window_from_column(
  container: &Container,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if !workspace.is_scrolling() {
    return Ok(());
  }

  let Ok(window) = container.as_tiling_container() else {
    return Ok(());
  };

  let is_stacked = window
    .parent()
    .and_then(|parent| parent.as_direction_container().ok())
    .is_some_and(|parent| {
      matches!(parent, DirectionContainer::Split(_))
    });

  if !is_stacked {
    return Ok(());
  }

  let Some(column) = workspace.focus_column(container) else {
    return Ok(());
  };

  let target_index = column.index() + 1;
  move_container_within_tree(
    &window.clone().into(),
    &workspace.clone().into(),
    target_index,
    state,
  )?;

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(())
}
