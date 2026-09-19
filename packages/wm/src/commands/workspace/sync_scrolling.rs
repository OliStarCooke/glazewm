use std::time::Instant;

use crate::{traits::CommonGetters, wm_state::WmState};

/// Syncs scroll offsets of scrolling workspaces before a redraw.
///
/// Persists clamped offsets for stale viewports and scrolls the
/// focused workspace to keep the focused column visible. Queues a
/// redraw of a workspace's columns whenever its target or offset
/// changed, or an animation is running.
pub fn sync_scrolling(state: &mut WmState) -> anyhow::Result<()> {
  let focused = state.focused_container();

  // Manual panning suppresses focus-following once so the viewport is
  // not snapped back in the same sync pass.
  let skip_auto_scroll = state.pending_sync.take_skip_auto_scroll();

  for workspace in state.workspaces() {
    if !workspace.is_scrolling() {
      continue;
    }

    let prev_offset = workspace.scroll_offset();
    let prev_target = workspace.scroll_target();

    // Re-clamp a stale target (e.g. after a column is removed or the
    // monitor is resized) without restarting the animation when the
    // clamped target is unchanged.
    workspace.set_target_scroll_offset(workspace.scroll_target());

    let focused_in_workspace = focused.as_ref().filter(|focused| {
      focused
        .workspace()
        .is_some_and(|focused_workspace| {
          focused_workspace.id() == workspace.id()
        })
    });

    // Manual panning suppresses focus-following so the viewport is
    // not snapped back in the same sync pass.
    if let Some(focused) = focused_in_workspace {
      if !skip_auto_scroll {
        workspace.ensure_visible(focused)?;
      }
    }

    if workspace.scroll_target() != prev_target
      || workspace.scroll_offset() != prev_offset
      || workspace.is_scroll_animating()
    {
      state
        .pending_sync
        .queue_containers_to_redraw(workspace.tiling_children());
    }
  }

  Ok(())
}

/// Advances running scroll animations and queues redraws.
///
/// Returns whether any workspace offset changed. Intended to be driven
/// by a frame ticker (see `main`), with `platform_sync` performing the
/// actual repositioning.
pub fn tick_scroll_animations(
  state: &mut WmState,
  now: Instant,
) -> anyhow::Result<bool> {
  let mut any_changed = false;

  for workspace in state.workspaces() {
    if !workspace.is_scrolling() || !workspace.is_scroll_animating() {
      continue;
    }

    if workspace.tick_scroll_animation(now) {
      any_changed = true;
      state
        .pending_sync
        .queue_containers_to_redraw(workspace.tiling_children());
    }
  }

  Ok(any_changed)
}
