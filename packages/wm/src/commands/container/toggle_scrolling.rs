use anyhow::Context;
use wm_common::{WmEvent, WorkspaceLayout};

use crate::{
  models::Container,
  traits::CommonGetters,
  user_config::UserConfig,
  wm_state::WmState,
};

/// Toggles a workspace between tiling and scrolling layouts.
pub fn toggle_scrolling(
  container: &Container,
  state: &mut WmState,
  _config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  let new_layout = match workspace.layout() {
    WorkspaceLayout::Tiling => WorkspaceLayout::Scrolling,
    WorkspaceLayout::Scrolling => WorkspaceLayout::Tiling,
  };

  set_workspace_layout(container, &new_layout, state)
}

/// Sets the layout of a workspace.
pub fn set_workspace_layout(
  container: &Container,
  layout: &WorkspaceLayout,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let workspace = container.workspace().context("No workspace.")?;

  if workspace.layout() == *layout {
    return Ok(());
  }

  workspace.set_layout(layout.clone());

  state
    .pending_sync
    .queue_container_to_redraw(workspace.clone());

  state.emit_event(WmEvent::WorkspaceUpdated {
    updated_workspace: workspace.to_dto()?,
  });

  Ok(())
}
