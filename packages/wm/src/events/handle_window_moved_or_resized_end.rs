use anyhow::Context;
use wm_common::{
  try_warn, FullscreenStateConfig, TilingDirection, WindowState,
};
use wm_platform::{LengthValue, Point, Rect};

use crate::{
  commands::{
    container::{move_container_within_tree, wrap_in_split_container},
    window::{set_window_size, update_window_state},
  },
  events::update_floating_window_position,
  models::{
    DirectionContainer, NonTilingWindow, SplitContainer,
    WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Handles the event for when a window is finished being moved or resized
/// by the user (e.g. via the window's drag handles).
///
/// This resizes the window if it's a tiling window and attach a dragged
/// floating window.
///
/// TODO: Move this to a better location - maybe a new `active_drag_ext`
/// mod.
pub fn handle_window_moved_or_resized_end(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    return Ok(());
  };

  match &window {
    WindowContainer::NonTilingWindow(window) => {
      let is_maximized = try_warn!(window.native().is_maximized());

      window.update_native_properties(|properties| {
        properties.is_maximized = is_maximized;
      });

      let nearest_monitor = state
        .nearest_monitor(&window.native())
        .context("Failed to get workspace of nearest monitor.")?;

      let should_fullscreen = window.should_fullscreen(
        &nearest_monitor
          .displayed_workspace()
          .context("No workspace.")?,
      )?;

      if is_maximized || should_fullscreen {
        let fullscreen_state = if let WindowState::Fullscreen(
          fullscreen_state,
        ) = window.state()
        {
          fullscreen_state
        } else {
          config
            .value
            .window_behavior
            .state_defaults
            .fullscreen
            .clone()
        };

        let window = update_window_state(
          window.clone().into(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: is_maximized,
            ..fullscreen_state
          }),
          state,
          config,
        )?;

        window.set_active_drag(None);

        if is_maximized {
          // Dequeue the window from redraw if it's maximized, since the
          // window is already in the correct state.
          state
            .pending_sync
            .dequeue_container_from_redraw(window.clone());
        } else {
          // Force a redraw to snap the window to the monitor edges.
          // TODO: Skip redraw if it's already matches fullscreen frame.
          state.pending_sync.queue_container_to_redraw(window.clone());
        }

        return Ok(());
      }

      if active_drag.is_from_floating {
        update_floating_window_position(
          window,
          window.native_properties().frame,
          &nearest_monitor,
          state,
        )?;
        window.set_active_drag(None);
      } else {
        // Window is a temporary floating window that should be
        // reverted back to tiling.
        let window = drop_as_tiling_window(window, state, config)?;
        window.set_active_drag(None);
      }
    }
    WindowContainer::TilingWindow(window) => {
      tracing::info!(
        "Tiling window move/resize ended: {}",
        window.as_window_container()?
      );

      let frame = window.native_properties().frame;

      // Update the window's size based on the new frame position. This
      // means we use the actual window dimensions as the source of truth.
      set_window_size(
        window.clone().into(),
        Some(LengthValue::from_px(frame.width())),
        Some(LengthValue::from_px(frame.height())),
        state,
      )?;

      window.set_active_drag(None);

      // Force a redraw of the window to snap it back to its original
      // position. This is necessary when:
      // - The window is the only tiling window in the workspace.
      // - The window is not past the movement threshold for transitioning
      //   to floating while being dragged.
      // - Resizing in a direction that doesn't change the window's tiling
      //   size.
      state.pending_sync.queue_container_to_redraw(window.clone());
    }
  }

  Ok(())
}

/// Handles transition from temporary floating window to tiling window on
/// drag end.
#[allow(clippy::too_many_lines)]
fn drop_as_tiling_window(
  moved_window: &NonTilingWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  tracing::debug!("Tiling window drag ended: {:?}", moved_window.id());

  let mouse_pos = state.dispatcher.cursor_position()?;
  let mouse_workspace = state
    .monitor_at_point(&mouse_pos)
    .and_then(|monitor| monitor.displayed_workspace())
    .or_else(|| moved_window.workspace())
    .context("Couldn't find workspace for window drop.")?;

  // Get the workspace, split containers, and other windows under the
  // dragged window.
  let containers_at_pos = state
    .containers_at_point(&mouse_workspace.clone().into(), &mouse_pos)
    .into_iter()
    .filter(|container| container.id() != moved_window.id());

  // Get the deepest direction container under the dragged window.
  // Depth is computed once per candidate (previously counted ancestors
  // of both sides on every fold comparison).
  let target_parent: DirectionContainer = containers_at_pos
    .filter_map(|container| container.as_direction_container().ok())
    .map(|container| {
      let depth = container.ancestors().count();
      (container, depth)
    })
    .fold(
      (mouse_workspace.into(), 0),
      |(acc, acc_depth), (container, depth)| {
        if depth > acc_depth {
          (container, depth)
        } else {
          (acc, acc_depth)
        }
      },
    )
    .0;

  // If the target parent has no children (i.e. an empty workspace), then
  // add the window directly.
  if target_parent.tiling_children().count() == 0 {
    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      0,
      state,
    )?;

    moved_window.set_insertion_target(None);

    return update_window_state(
      moved_window.as_window_container()?,
      WindowState::Tiling,
      state,
      config,
    );
  }

  // Precompute rects once (previously `to_rect` ran per comparison for
  // both the accumulator and the candidate, making selection O(n) rect
  // builds of the same containers).
  let tiling_children = target_parent
    .children()
    .into_iter()
    .filter_map(|container| container.as_tiling_container().ok())
    .collect::<Vec<_>>();

  let mut rect_by_id =
    std::collections::HashMap::with_capacity(tiling_children.len());

  for child in &tiling_children {
    rect_by_id.insert(child.id(), child.to_rect()?);
  }

  let nearest_container = tiling_children
    .into_iter()
    .min_by(|a, b| {
      let dist_a = rect_by_id
        .get(&a.id())
        .map(|rect| rect.distance_to_point(&mouse_pos))
        .unwrap_or(f32::MAX);
      let dist_b = rect_by_id
        .get(&b.id())
        .map(|rect| rect.distance_to_point(&mouse_pos))
        .unwrap_or(f32::MAX);

      dist_a.partial_cmp(&dist_b).unwrap_or(std::cmp::Ordering::Equal)
    })
    .context("No nearest container.")?;

  let tiling_direction = target_parent.tiling_direction();
  let nearest_rect =
    rect_by_id.get(&nearest_container.id()).context("No rect.")?;
  let drop_position = drop_position(&mouse_pos, nearest_rect);

  let moved_window = update_window_state(
    moved_window.clone().into(),
    WindowState::Tiling,
    state,
    config,
  )?;

  let should_split = nearest_container.is_tiling_window()
    && match tiling_direction {
      TilingDirection::Horizontal => {
        drop_position == DropPosition::Top
          || drop_position == DropPosition::Bottom
      }
      TilingDirection::Vertical => {
        drop_position == DropPosition::Left
          || drop_position == DropPosition::Right
      }
    };

  if should_split {
    let split_container = SplitContainer::new(
      tiling_direction.inverse(),
      config.value.gaps.clone(),
    );

    wrap_in_split_container(
      &split_container,
      &target_parent.clone().into(),
      &[nearest_container],
    )?;

    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => 0,
      _ => 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &split_container.into(),
      target_index,
      state,
    )?;
  } else {
    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => nearest_container.index(),
      _ => nearest_container.index() + 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      target_index,
      state,
    )?;
  }

  state.pending_sync.queue_container_to_redraw(target_parent);

  Ok(moved_window)
}

/// Represents where the window was dropped over another.
#[derive(Debug, Clone, PartialEq)]
enum DropPosition {
  Top,
  Bottom,
  Left,
  Right,
}

/// Gets the drop position for a window based on the mouse position.
///
/// This approach divides the window rect into an "X", creating four
/// triangular quadrants, to determine which side the cursor is closest to.
fn drop_position(mouse_pos: &Point, rect: &Rect) -> DropPosition {
  let delta_x = mouse_pos.x - rect.center_point().x;
  let delta_y = mouse_pos.y - rect.center_point().y;

  if delta_x.abs() > delta_y.abs() {
    // Window is in the left or right triangle.
    if delta_x > 0 {
      DropPosition::Right
    } else {
      DropPosition::Left
    }
  } else {
    // Window is in the top or bottom triangle.
    if delta_y > 0 {
      DropPosition::Bottom
    } else {
      DropPosition::Top
    }
  }
}
