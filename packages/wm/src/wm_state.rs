use std::time::Instant;

use anyhow::Context;
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
use wm_common::{BindingModeConfig, HideCorner, WindowState, WmEvent};
use wm_platform::{
  Direction, Dispatcher, Display, NativeWindow, Point, Rect,
};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, OpacityValue};

use crate::{
  commands::{
    container::set_focused_descendant,
    general::platform_sync,
    monitor::{add_monitor, move_bounded_workspaces_to_new_monitor},
    window::{manage_window, unmanage_window},
  },
  models::{
    Container, Monitor, NativeMonitorProperties, RootContainer,
    WindowContainer, Workspace, WorkspaceTarget,
  },
  pending_sync::PendingSync,
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
};

pub struct WmState {
  /// Root node of the container tree. Monitors are the children of the
  /// root node, followed by workspaces, then split containers/windows.
  pub root_container: RootContainer,

  pub dispatcher: Dispatcher,

  pub pending_sync: PendingSync,

  /// Name of the most recently focused workspace.
  ///
  /// Used for the `general.toggle_workspace_on_refocus` option on
  /// workspace focus.
  pub recent_workspace_name: Option<String>,

  /// The previously focused window that had focus effects applied.
  ///
  /// Used to efficiently update window effects by only removing focus
  /// effects from the previous window rather than all windows when focus
  /// changes.
  pub prev_effects_window: Option<WindowContainer>,

  /// Time since a previously focused window was unmanaged or minimized.
  ///
  /// Used to decide whether to override incoming focus events.
  pub unmanaged_or_minimized_timestamp: Option<Instant>,

  /// Configs of currently enabled binding modes.
  pub binding_modes: Vec<BindingModeConfig>,

  /// Windows that the WM should ignore. Windows can be added via the
  /// `ignore` command.
  pub ignored_windows: Vec<NativeWindow>,

  /// Whether the WM is paused.
  pub is_paused: bool,

  /// Whether the OS focused window is the same as the WM focused window.
  pub is_focus_synced: bool,

  /// Whether the initial state has been populated.
  has_initialized: bool,

  /// Cached hide-corner result keyed by monitor working areas.
  ///
  /// Recomputed only when monitor ids or working areas change (previously
  /// O(m²) per sync).
  hide_corner_cache_key: Vec<(Uuid, Rect)>,
  hide_corner_cache: Vec<(Monitor, HideCorner)>,

  /// Id of the container in the last emitted `FocusChanged` event.
  ///
  /// Skips rebuilding the recursive DTO + broadcast when focus is
  /// re-emitted for the same container.
  last_emitted_focused_id: Option<Uuid>,

  /// Sender for emitting WM-related events.
  event_tx: mpsc::UnboundedSender<WmEvent>,

  /// Sender for gracefully shutting down the WM.
  exit_tx: mpsc::UnboundedSender<()>,
}

impl WmState {
  pub fn new(
    dispatcher: Dispatcher,
    event_tx: mpsc::UnboundedSender<WmEvent>,
    exit_tx: mpsc::UnboundedSender<()>,
  ) -> Self {
    Self {
      root_container: RootContainer::new(),
      dispatcher,
      pending_sync: PendingSync::default(),
      prev_effects_window: None,
      recent_workspace_name: None,
      unmanaged_or_minimized_timestamp: None,
      binding_modes: Vec::new(),
      ignored_windows: Vec::new(),
      is_paused: false,
      is_focus_synced: false,
      has_initialized: false,
      hide_corner_cache_key: Vec::new(),
      hide_corner_cache: Vec::new(),
      last_emitted_focused_id: None,
      event_tx,
      exit_tx,
    }
  }

  /// Populates the initial WM state by creating containers for all
  /// existing windows and monitors.
  pub fn populate(
    &mut self,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // Get the originally focused window when the WM was started.
    let focused_window = self.dispatcher.focused_window().ok();

    // Create a monitor, and consequently a workspace, for each detected
    // native monitor.
    for native_display in self.dispatcher.sorted_displays()? {
      if let Ok(native_properties) =
        NativeMonitorProperties::try_from(&native_display)
      {
        let monitor =
          add_monitor(native_display, native_properties, self)?;
        move_bounded_workspaces_to_new_monitor(&monitor, self, config)?;
      }
    }

    // Manage windows in reverse z-order (bottom to top). This helps to
    // preserve the original stacking order.
    for native_window in
      self.dispatcher.visible_windows()?.into_iter().rev()
    {
      let nearest_workspace = self
        .nearest_monitor(&native_window)
        .and_then(|m| m.displayed_workspace());

      if let Some(workspace) = nearest_workspace {
        manage_window(
          native_window,
          Some(workspace.into()),
          self,
          config,
        )?;
      }
    }

    let container_to_focus = focused_window
      .and_then(|focused_window| {
        self.window_from_native(&focused_window).map(Into::into)
      })
      .or_else(|| self.windows().pop().map(Into::into))
      .or_else(|| self.workspaces().pop().map(Into::into))
      .context("Failed to get container to focus.")?;

    set_focused_descendant(&container_to_focus, None);
    self.is_focus_synced = true;

    self
      .pending_sync
      .queue_focus_change()
      .queue_all_effects_update();

    for workspace in self.workspaces() {
      self.pending_sync.queue_workspace_to_reorder(workspace);
    }

    platform_sync(self, config)?;
    self.has_initialized = true;

    Ok(())
  }

  pub fn monitors(&self) -> Vec<Monitor> {
    self.root_container.monitors()
  }

  pub fn workspaces(&self) -> Vec<Workspace> {
    self
      .monitors()
      .iter()
      .flat_map(Monitor::workspaces)
      .collect()
  }

  /// Gets workspaces sorted by their position in the user config.
  pub fn sorted_workspaces(&self, config: &UserConfig) -> Vec<Workspace> {
    let mut workspaces = self.workspaces();
    config.sort_workspaces(&mut workspaces);
    workspaces
  }

  pub fn windows(&self) -> Vec<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the monitor that encompasses the largest portion of a given
  /// window.
  ///
  /// Defaults to the first monitor if the nearest monitor is invalid.
  pub fn nearest_monitor(
    &self,
    native_window: &NativeWindow,
  ) -> Option<Monitor> {
    self
      .monitor_from_native(
        &self.dispatcher.nearest_display(native_window).ok()?,
      )
      .or(self.monitors().first().cloned())
  }

  /// Gets monitor that corresponds to the given `Display`.
  pub fn monitor_from_native(
    &self,
    native_display: &Display,
  ) -> Option<Monitor> {
    self
      .monitors()
      .into_iter()
      .find(|monitor| monitor.native() == *native_display)
  }

  /// Gets the closest monitor in a given direction.
  ///
  /// Uses i3wm's algorithm for finding best guess.
  pub fn monitor_in_direction(
    &self,
    origin_monitor: &Monitor,
    direction: &Direction,
  ) -> anyhow::Result<Option<Monitor>> {
    let origin_rect = origin_monitor.native_properties().bounds;

    // Create a tuple of monitors and their rect.
    let monitors_with_rect = self
      .monitors()
      .into_iter()
      .map(|monitor| {
        let rect = monitor.native_properties().bounds;
        anyhow::Ok((monitor, rect))
      })
      .try_collect::<Vec<_>>()?;

    let closest_monitor = monitors_with_rect
      .into_iter()
      .filter(|(_, rect)| match direction {
        Direction::Right => {
          rect.x() > origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Left => {
          rect.x() < origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Down => {
          rect.y() > origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
        Direction::Up => {
          rect.y() < origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
      })
      .min_by(|(_, rect_a), (_, rect_b)| match direction {
        Direction::Right => rect_a.x().cmp(&rect_b.x()),
        Direction::Left => rect_b.x().cmp(&rect_a.x()),
        Direction::Down => rect_a.y().cmp(&rect_b.y()),
        Direction::Up => rect_b.y().cmp(&rect_a.y()),
      })
      .map(|(monitor, _)| monitor);

    Ok(closest_monitor)
  }

  /// Determines the preferred hide corner for each monitor. Used for
  /// [`HideMethod::PlaceInCorner`].
  ///
  /// The corner is chosen by simulating a 400x400 window frame in the
  /// bottom-left and bottom-right of the monitor's working area, then
  /// picking the side that overlaps the least with other monitors'
  /// working areas (ties favor bottom-right).
  pub fn monitors_by_hide_corner(&mut self) -> Vec<(Monitor, HideCorner)> {
    const TEST_FRAME_SIZE: i32 = 400;
    const VISIBLE_SLIVER: i32 = 1;

    let monitors = self.monitors();
    let cache_key = monitors
      .iter()
      .map(|monitor| {
        (monitor.id(), monitor.native_properties().working_area)
      })
      .collect::<Vec<_>>();

    // Reuse cached corners when monitor layout is unchanged.
    if cache_key == self.hide_corner_cache_key {
      return self.hide_corner_cache.clone();
    }

    let working_areas = monitors
      .iter()
      .map(|monitor| monitor.native_properties().working_area)
      .collect::<Vec<_>>();

    let result = monitors
      .into_iter()
      .enumerate()
      .map(|(idx, monitor)| {
        let monitor_rect = &working_areas[idx];
        let test_frame_y = monitor_rect.bottom - TEST_FRAME_SIZE;

        let left_test_frame = Rect::from_xy(
          monitor_rect.left - TEST_FRAME_SIZE + VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let right_test_frame = Rect::from_xy(
          monitor_rect.right - VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let overlap_area = |test_frame: &Rect| -> i32 {
          working_areas
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, rect)| test_frame.intersection_area(rect))
            .sum()
        };

        let left_overlap = overlap_area(&left_test_frame);
        let right_overlap = overlap_area(&right_test_frame);

        let corner = if left_overlap < right_overlap {
          HideCorner::BottomLeft
        } else {
          HideCorner::BottomRight
        };

        (monitor, corner)
      })
      .collect::<Vec<_>>();

    self.hide_corner_cache_key = cache_key;
    self.hide_corner_cache.clone_from(&result);
    result
  }

  /// Gets window that corresponds to the given `NativeWindow`.
  pub fn window_from_native(
    &self,
    native_window: &NativeWindow,
  ) -> Option<WindowContainer> {
    // Walk descendants directly instead of `windows()` to avoid an
    // intermediate `Vec` alloc on this hot path.
    self
      .root_container
      .descendants()
      .filter_map(|container| container.as_window_container().ok())
      .find(|window| &*window.native() == native_window)
  }

  /// Whether any window currently has an active drag operation.
  ///
  /// Short-circuits on the first match instead of collecting all windows.
  pub fn has_active_drag(&self) -> bool {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.as_window_container().ok())
      .any(|window| window.active_drag().is_some())
  }

  /// Windows with an active drag operation (usually 0 or 1).
  ///
  /// Walks descendants directly to avoid a full `windows()` collect.
  pub fn active_drag_windows(&self) -> Vec<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.as_window_container().ok())
      .filter(|window| window.active_drag().is_some())
      .collect()
  }

  /// Gets window with the given native window id.
  ///
  /// Walks descendants directly to avoid a `windows()` collect on the
  /// per-pixel mouse-move path.
  pub fn window_from_native_id(
    &self,
    native_id: wm_platform::WindowId,
  ) -> Option<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.as_window_container().ok())
      .find(|window| window.native().id() == native_id)
  }

  pub fn workspace_by_name(
    &self,
    workspace_name: &str,
  ) -> Option<Workspace> {
    self
      .workspaces()
      .into_iter()
      .find(|workspace| workspace.config().name == workspace_name)
  }

  /// Gets a workspace and its name by the given target.
  ///
  /// Returns a tuple of the workspace name and the `Workspace` instance
  /// if active.
  #[allow(clippy::too_many_lines)]
  pub fn workspace_by_target(
    &self,
    origin_workspace: &Workspace,
    target: WorkspaceTarget,
    config: &UserConfig,
  ) -> anyhow::Result<(Option<String>, Option<Workspace>)> {
    let (name, workspace) = match target {
      WorkspaceTarget::Name(name) => {
        #[allow(clippy::match_bool)]
        match origin_workspace.config().name == name {
          false => (Some(name.clone()), self.workspace_by_name(&name)),
          // Toggle the workspace if it's already focused.
          true if config.value.general.toggle_workspace_on_refocus => (
            self.recent_workspace_name.clone(),
            self
              .recent_workspace_name
              .as_ref()
              .and_then(|name| self.workspace_by_name(name)),
          ),
          true => (None, None),
        }
      }
      WorkspaceTarget::Recent => (
        self.recent_workspace_name.clone(),
        self
          .recent_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name)),
      ),
      WorkspaceTarget::NextActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let next_active_workspace = active_workspaces
          .get(origin_index + 1)
          .or_else(|| active_workspaces.first());

        (
          next_active_workspace.map(|workspace| workspace.config().name),
          next_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::PreviousActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let prev_active_workspace = active_workspaces.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(active_workspaces.len() - 1),
        );

        (
          prev_active_workspace.map(|workspace| workspace.config().name),
          prev_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::NextActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let next_active_workspace_in_monitor = workspace_in_monitor
          .get(origin_index + 1)
          .or_else(|| workspace_in_monitor.first());

        (
          next_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          next_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::PreviousActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let prev_active_workspace_in_monitor = workspace_in_monitor.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(workspace_in_monitor.len() - 1),
        );

        (
          prev_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          prev_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::Next => {
        let origin_name = origin_workspace.config().name;
        let origin_index = config
          .workspace_config_index(&origin_name)
          .context("Failed to get index of given workspace.")?;

        let workspaces = &config.value.workspaces;
        let next_workspace_config = workspaces
          .get(origin_index + 1)
          .or_else(|| workspaces.first());

        let next_workspace_name =
          next_workspace_config.map(|config| config.name.clone());

        let next_workspace = next_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (next_workspace_name, next_workspace)
      }
      WorkspaceTarget::Previous => {
        let origin_name = origin_workspace.config().name;
        let origin_index = config
          .workspace_config_index(&origin_name)
          .context("Failed to get index of given workspace.")?;

        let workspaces = &config.value.workspaces;
        let previous_workspace_config = workspaces.get(
          origin_index.checked_sub(1).unwrap_or(workspaces.len() - 1),
        );

        let previous_workspace_name =
          previous_workspace_config.map(|config| config.name.clone());

        let previous_workspace = previous_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (previous_workspace_name, previous_workspace)
      }

      WorkspaceTarget::Direction(direction) => {
        let origin_monitor =
          origin_workspace.monitor().context("No focused monitor.")?;

        let target_workspace = self
          .monitor_in_direction(&origin_monitor, &direction)?
          .and_then(|monitor| monitor.displayed_workspace());

        (
          target_workspace
            .as_ref()
            .map(|workspace| workspace.config().name),
          target_workspace,
        )
      }
    };

    Ok((name, workspace))
  }

  /// Gets windows that should be redrawn.
  ///
  /// When redrawing after a command that changes a window's type (e.g.
  /// tiling -> floating), the original detached window might still be
  /// queued for a redraw and should be filtered out.
  pub fn windows_to_redraw(&self) -> Vec<WindowContainer> {
    self
      .pending_sync
      .containers_to_redraw()
      .values()
      .flat_map(CommonGetters::self_and_descendants)
      .filter(|container| !container.is_detached())
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the currently focused container. This can either be a window or
  /// a workspace without any descendant windows.
  ///
  /// Follows the first child in focus order down from the root (O(depth))
  /// instead of walking the full focus order.
  pub fn focused_container(&self) -> Option<Container> {
    let mut current = self.root_container.as_container();

    loop {
      // First resolvable child in focus order (skips stale ids).
      let next = current
        .borrow_child_focus_order()
        .iter()
        .find_map(|id| current.child_by_id(id))?;

      if next.has_children() {
        current = next;
      } else {
        return Some(next);
      }
    }
  }

  /// Emits a WM event through an MSPC channel.
  ///
  /// Does not emit events while the WM is paused or populating initial
  /// state. This is to prevent events (e.g. workspace activation events)
  /// from being emitted via IPC server before the initial state is
  /// prepared.
  pub fn emit_event(&self, event: WmEvent) {
    if self.has_initialized
      && (!self.is_paused || matches!(event, WmEvent::PauseChanged { .. }))
      && !self.event_tx.is_closed()
    {
      if let Err(err) = self.event_tx.send(event) {
        warn!("Failed to send event: {}", err);
      }
    }
  }

  /// Emits a `FocusChanged` event unless the same container was already
  /// announced in the previous emission.
  ///
  /// Skips the recursive `to_dto` + broadcast on duplicate focus
  /// announcements (e.g. focus sync queuing plus the native focus event
  /// for the same window).
  pub fn emit_focus_changed(
    &mut self,
    container: &Container,
  ) -> anyhow::Result<()> {
    if self.last_emitted_focused_id == Some(container.id()) {
      return Ok(());
    }

    self.last_emitted_focused_id = Some(container.id());
    let focused_container = container.to_dto()?;
    self.emit_event(WmEvent::FocusChanged { focused_container });

    Ok(())
  }

  /// Starts graceful shutdown via an MSPC channel.
  pub fn emit_exit(&self) -> anyhow::Result<()> {
    self.exit_tx.send(())?;
    Ok(())
  }

  pub fn container_by_id(&self, id: Uuid) -> Option<Container> {
    self
      .root_container
      .self_and_descendants()
      .find(|container| container.id() == id)
  }

  /// Gets container to focus after the given window is unmanaged,
  /// minimized, or moved to another workspace.
  pub fn focus_target_after_removal(
    &self,
    removed_window: &WindowContainer,
  ) -> Option<Container> {
    // If the removed window is not focused, no need to change focus.
    if self.focused_container() != Some(removed_window.clone().into()) {
      return None;
    }

    // Get descendant focus order excluding the removed container.
    let workspace = removed_window.workspace()?;
    let removed_state = removed_window.state();

    // Single pass over focus order (previously collected the full order
    // then scanned it 3x).
    let mut first_of_type = None;
    let mut first_non_minimized = None;
    let mut first = None;

    for descendant in workspace
      .descendant_focus_order()
      .filter(|descendant| descendant.id() != removed_window.id())
    {
      if first.is_none() {
        first = Some(descendant.clone());
      }

      if let Ok(descendant_window) = descendant.as_window_container() {
        let descendant_state = descendant_window.state();

        if first_of_type.is_none() {
          let same_type = matches!(
            (&descendant_state, &removed_state),
            (WindowState::Tiling, WindowState::Tiling)
              | (WindowState::Floating(_), WindowState::Floating(_))
              | (WindowState::Fullscreen(_), WindowState::Fullscreen(_))
          );

          if same_type {
            first_of_type = Some(descendant.clone().into());
          }
        }

        if first_non_minimized.is_none()
          && descendant_state != WindowState::Minimized
        {
          first_non_minimized = Some(descendant.clone().into());
        }
      }

      if first_of_type.is_some() {
        break;
      }
    }

    // Get focus target that matches the removed window type. This applies
    // for windows that aren't in a minimized state.
    if first_of_type.is_some() {
      return first_of_type;
    }

    first_non_minimized
      .or(first)
      .or(Some(workspace.into()))
  }

  /// Returns all containers that contain the given point.
  #[allow(clippy::unused_self)]
  pub fn containers_at_point(
    &self,
    origin_container: &Container,
    point: &Point,
  ) -> Vec<Container> {
    origin_container
      .descendants()
      .filter(|descendant| {
        descendant
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .collect()
  }

  /// Returns the monitor that contains the given point.
  pub fn monitor_at_point(&self, point: &Point) -> Option<Monitor> {
    self
      .monitors()
      .iter()
      .find(|monitor| {
        monitor
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .cloned()
  }

  /// Cleans up windows that are no longer alive.
  ///
  /// This addresses the "ghost window" issue where applications may
  /// terminate without sending window destroy events, leaving invalid
  /// windows in WM state.
  ///
  /// See: <https://github.com/glzr-io/glazewm/issues/1219>
  pub fn cleanup_invalid_windows(&mut self) -> anyhow::Result<()> {
    let invalid_windows = self
      .windows()
      .into_iter()
      .filter(|window| !window.native().is_valid());

    for window in invalid_windows {
      tracing::debug!("Removing invalid window: {:?}", window.id());
      unmanage_window(window, self)?;
    }

    // Prune ignored windows that are no longer valid.
    self.ignored_windows.retain(NativeWindow::is_valid);

    Ok(())
  }
}

impl Drop for WmState {
  fn drop(&mut self) {
    let managed_windows = self.windows();

    for window in &managed_windows {
      // Redraw windows to their intended positions. On macOS, this will
      // unhide windows that are on other workspaces.
      if let Ok(rect) = window.to_rect() {
        if let Err(err) = window.native().set_frame(&rect) {
          warn!("Failed to redraw window on cleanup: {:?}", err);
        }
      }

      // Reset any effects on Windows.
      #[cfg(target_os = "windows")]
      {
        if let Err(err) = window.native().show() {
          warn!("Failed to show window: {:?}", err);
        }

        let _ = window.native().set_taskbar_visibility(true);
        let _ = window.native().set_border_color(None);
        let _ = window
          .native()
          .set_transparency(&OpacityValue::from_alpha(u8::MAX));
      }
    }
  }
}
