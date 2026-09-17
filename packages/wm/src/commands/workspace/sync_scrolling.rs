use crate::{traits::CommonGetters, wm_state::WmState};

/// Syncs scroll offsets of scrolling workspaces before a redraw.
///
/// Persists clamped offsets for stale viewports and scrolls the
/// focused workspace to keep the focused column visible. Queues a
/// redraw of a workspace's columns whenever its offset changed.
pub fn sync_scrolling(state: &mut WmState) -> anyhow::Result<()> {
  let focused = state.focused_container();

  for workspace in state.workspaces() {
    if !workspace.is_scrolling() {
      continue;
    }

    // Persist the clamped offset so stale viewports (e.g. after a
    // column is removed or the monitor is resized) self-heal.
    workspace.set_scroll_offset(workspace.scroll_offset());

    let prev_offset = workspace.scroll_offset();

    let focused_in_workspace = focused.as_ref().filter(|focused| {
      focused
        .workspace()
        .is_some_and(|focused_workspace| {
          focused_workspace.id() == workspace.id()
        })
    });

    if let Some(focused) = focused_in_workspace {
      workspace.ensure_visible(focused)?;
    }

    if workspace.scroll_offset() != prev_offset {
      state
        .pending_sync
        .queue_containers_to_redraw(workspace.tiling_children());
    }
  }

  Ok(())
}
