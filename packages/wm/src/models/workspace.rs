use std::{
  cell::{Ref, RefCell, RefMut},
  collections::VecDeque,
  rc::Rc,
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  ContainerDto, GapsConfig, TilingDirection, WorkspaceConfig,
  WorkspaceDto, WorkspaceLayout,
};
use wm_platform::{Rect, RectDelta};

use crate::{
  impl_common_getters, impl_container_debug,
  impl_tiling_direction_getters,
  models::{
    Container, DirectionContainer, TilingContainer, WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters,
  },
};

#[derive(Clone)]
pub struct Workspace(Rc<RefCell<WorkspaceInner>>);

#[derive(Debug)]
struct WorkspaceInner {
  id: Uuid,
  parent: Option<Container>,
  children: VecDeque<Container>,
  child_focus_order: VecDeque<Uuid>,
  config: WorkspaceConfig,
  gaps_config: GapsConfig,
  tiling_direction: TilingDirection,
  layout: WorkspaceLayout,
  scroll_offset: i32,
}

impl Workspace {
  pub fn new(
    config: WorkspaceConfig,
    gaps_config: GapsConfig,
    tiling_direction: TilingDirection,
  ) -> Self {
    // Scrolling workspaces always use a horizontal strip of columns.
    let tiling_direction = match config.layout {
      WorkspaceLayout::Scrolling => TilingDirection::Horizontal,
      WorkspaceLayout::Tiling => tiling_direction,
    };

    let workspace = WorkspaceInner {
      id: Uuid::new_v4(),
      parent: None,
      children: VecDeque::new(),
      child_focus_order: VecDeque::new(),
      layout: config.layout.clone(),
      scroll_offset: 0,
      config,
      gaps_config,
      tiling_direction,
    };

    Self(Rc::new(RefCell::new(workspace)))
  }

  /// Underlying config for the workspace.
  pub fn config(&self) -> WorkspaceConfig {
    self.0.borrow().config.clone()
  }

  /// Update the underlying config for the workspace.
  pub fn set_config(&self, config: WorkspaceConfig) {
    self.set_layout(config.layout.clone());
    self.0.borrow_mut().config = config;
  }

  /// Whether the workspace is currently displayed by the parent monitor.
  pub fn is_displayed(&self) -> bool {
    self
      .monitor()
      .and_then(|monitor| monitor.displayed_workspace())
      .is_some_and(|workspace| workspace.id() == self.id())
  }

  /// Layout of the workspace.
  pub fn layout(&self) -> WorkspaceLayout {
    self.0.borrow().layout.clone()
  }

  /// Whether the workspace uses niri-style scrolling columns.
  pub fn is_scrolling(&self) -> bool {
    matches!(self.layout(), WorkspaceLayout::Scrolling)
  }

  /// Sets the layout of the workspace.
  ///
  /// Scrolling workspaces always use a horizontal strip, so enabling
  /// scrolling forces the tiling direction to horizontal. The scroll
  /// offset is reset.
  pub fn set_layout(&self, layout: WorkspaceLayout) {
    if self.0.borrow().layout == layout {
      return;
    }

    if matches!(layout, WorkspaceLayout::Scrolling) {
      self.0.borrow_mut().tiling_direction = TilingDirection::Horizontal;
    }

    self.0.borrow_mut().layout = layout;
    self.0.borrow_mut().scroll_offset = 0;
  }

  /// Horizontal scroll offset of the viewport in pixels.
  ///
  /// Clamped to the valid range, so stale offsets (e.g. after a column
  /// is removed or the monitor is resized) never affect layout.
  pub fn scroll_offset(&self) -> i32 {
    let offset = self.0.borrow().scroll_offset;
    let max_offset = self.max_scroll_offset().unwrap_or(i32::MAX);
    offset.clamp(0, max_offset)
  }

  /// Sets the scroll offset clamped to the valid range.
  pub fn set_scroll_offset(&self, offset: i32) {
    let max_offset = self.max_scroll_offset().unwrap_or(0);
    self.0.borrow_mut().scroll_offset = offset.clamp(0, max_offset);
  }

  /// Maximum scroll offset such that the end of the strip aligns with
  /// the end of the viewport.
  pub fn max_scroll_offset(&self) -> anyhow::Result<i32> {
    if !self.is_scrolling() {
      return Ok(0);
    }

    let viewport = self.to_rect()?;
    let total_width = self.total_strip_width()?;
    Ok((total_width - viewport.width()).max(0))
  }

  /// Total width of the column strip, including inner gaps.
  fn total_strip_width(&self) -> anyhow::Result<i32> {
    let columns = self.tiling_children().collect::<Vec<_>>();
    if columns.is_empty() {
      return Ok(0);
    }

    let inner_gap = self.scrolling_inner_gap()?;
    let mut total = 0;
    for column in &columns {
      total += self.scrolling_column_width(column)?;
    }
    #[allow(clippy::cast_possible_wrap)]
    let gap_count = columns.len() as i32 - 1;
    total += inner_gap * gap_count;
    Ok(total)
  }

  /// Width of a top-level column as a fraction of the viewport width.
  ///
  /// Unlike tiling workspaces, the width is stable and independent of
  /// sibling columns.
  pub fn scrolling_column_width(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<i32> {
    let viewport = self.to_rect()?;

    #[allow(
      clippy::cast_precision_loss,
      clippy::cast_possible_truncation,
      clippy::cast_possible_wrap
    )]
    Ok((column.tiling_size() * viewport.width() as f32).round() as i32)
  }

  /// X-coordinate of a column before applying the scroll offset.
  fn column_unscrolled_x(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<i32> {
    let viewport = self.to_rect()?;
    let inner_gap = self.scrolling_inner_gap()?;

    let mut x = viewport.x();
    for sibling in self.tiling_children() {
      if sibling.id() == column.id() {
        break;
      }
      x += self.scrolling_column_width(&sibling)? + inner_gap;
    }
    Ok(x)
  }

  /// X-coordinate of a column with the scroll offset applied.
  ///
  /// The coordinate can lie outside the viewport when the column is
  /// scrolled out of view.
  pub fn scrolling_column_x(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<i32> {
    Ok(self.column_unscrolled_x(column)? - self.scroll_offset())
  }

  /// Horizontal inner gap in pixels.
  fn scrolling_inner_gap(&self) -> anyhow::Result<i32> {
    let monitor = self.monitor().context("Workspace has no monitor.")?;
    let monitor_rect = monitor.native_properties().bounds;
    let gaps_config = &self.0.borrow().gaps_config;

    let scale_factor = if gaps_config.scale_with_dpi {
      monitor.native_properties().scale_factor
    } else {
      1.
    };

    Ok(
      gaps_config
        .inner_gap
        .to_px(monitor_rect.height(), Some(scale_factor)),
    )
  }

  /// Rect of a top-level column in a scrolling workspace.
  ///
  /// Columns keep stable widths on a horizontal strip. The scroll
  /// offset shifts them relative to the viewport.
  pub fn scrolling_column_rect(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<Rect> {
    let viewport = self.to_rect()?;
    let width = self.scrolling_column_width(column)?;
    let x = self.scrolling_column_x(column)?;
    Ok(Rect::from_xy(x, viewport.y(), width, viewport.height()))
  }

  /// Top-level column that contains the given container.
  ///
  /// Returns `None` if the container is not a descendant of this
  /// workspace.
  pub fn focus_column(
    &self,
    container: &Container,
  ) -> Option<TilingContainer> {
    container.self_and_ancestors().find_map(|ancestor| {
      let column = ancestor.as_tiling_container().ok()?;
      column
        .parent()
        .filter(|parent| parent.id() == self.id())
        .map(|_| column)
    })
  }

  /// Whether a column is at least partially inside the viewport.
  pub fn is_column_visible(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<bool> {
    let viewport = self.to_rect()?;
    let x = self.scrolling_column_x(column)?;
    let width = self.scrolling_column_width(column)?;
    Ok(x + width > viewport.x() && x < viewport.x() + viewport.width())
  }

  /// Whether a window is scrolled out of the viewport.
  ///
  /// Only applies to tiling windows in scrolling workspaces. Floating
  /// and fullscreen windows are always considered visible.
  pub fn is_window_scrolled_out(
    &self,
    window: &WindowContainer,
  ) -> bool {
    if !self.is_scrolling() {
      return false;
    }

    let container: Container = window.clone().into();
    match self.focus_column(&container) {
      Some(column) => !self.is_column_visible(&column).unwrap_or(true),
      None => false,
    }
  }

  /// Scrolls the viewport the minimum amount needed to fully show the
  /// column containing the given container.
  ///
  /// Returns whether the offset changed.
  pub fn ensure_visible(
    &self,
    container: &Container,
  ) -> anyhow::Result<bool> {
    if !self.is_scrolling() {
      return Ok(false);
    }

    let Some(column) = self.focus_column(container) else {
      return Ok(false);
    };

    let viewport = self.to_rect()?;
    let unscrolled_x = self.column_unscrolled_x(&column)?;
    let width = self.scrolling_column_width(&column)?;
    let offset = self.scroll_offset();

    let x = unscrolled_x - offset;
    let new_offset = if x < viewport.x() {
      unscrolled_x - viewport.x()
    } else if x + width > viewport.x() + viewport.width() {
      unscrolled_x + width - (viewport.x() + viewport.width())
    } else {
      return Ok(false);
    };

    let prev_offset = self.scroll_offset();
    self.set_scroll_offset(new_offset);
    Ok(self.scroll_offset() != prev_offset)
  }

  pub fn set_gaps_config(&self, gaps_config: GapsConfig) {
    self.0.borrow_mut().gaps_config = gaps_config;
  }

  /// Effective outer gaps for this workspace.
  ///
  /// Uses `single_window_outer_gap` when the workspace has a single tiling
  /// window, otherwise falls back to `outer_gap`.
  pub fn outer_gaps(&self) -> RectDelta {
    let is_single_window = self.tiling_children().nth(1).is_none();

    let gaps_config = &self.0.borrow().gaps_config;
    let gaps = if is_single_window {
      gaps_config
        .single_window_outer_gap
        .as_ref()
        .unwrap_or(&gaps_config.outer_gap)
    } else {
      &gaps_config.outer_gap
    };

    // TODO: Should this be scaled by the monitor's DPI?
    gaps.clone()
  }

  /// Gets the bounds of a workspace with the given outer gap config.
  fn workspace_rect_with_gap_config(
    &self,
    outer_gaps: &RectDelta,
  ) -> anyhow::Result<Rect> {
    let monitor =
      self.monitor().context("Workspace has no parent monitor.")?;

    let gaps_config = &self.0.borrow().gaps_config;
    let scale_factor = if gaps_config.scale_with_dpi {
      monitor.native_properties().scale_factor
    } else {
      1.
    };

    // Get the delta between the monitor's bounds and its working area.
    let monitor_bounds = monitor.native_properties().bounds;
    let working_area_delta = monitor
      .native_properties()
      .working_area
      .delta(&monitor_bounds);

    Ok(
      monitor_bounds
        // Scale the gaps if `scale_with_dpi` is enabled. Outer gap config
        // values can be a percentage (relative to the monitor bounds), so
        // the outer gap delta needs to be applied prior to the working
        // area delta.
        .apply_delta(&outer_gaps.inverse(), Some(scale_factor))
        .apply_delta(&working_area_delta, None),
    )
  }

  /// Gets the maximum bounds of a workspace considering both `outer_gap`
  /// and `single_window_outer_gap` config values.
  pub fn max_workspace_rect(&self) -> anyhow::Result<Rect> {
    let gaps_config = &self.0.borrow().gaps_config;

    // Get the workspace rect using `outer_gap`.
    let multi_window_rect =
      self.workspace_rect_with_gap_config(&gaps_config.outer_gap)?;

    let Some(single_gap) = &gaps_config.single_window_outer_gap else {
      return Ok(multi_window_rect);
    };

    // Get the workspace rect using `single_window_outer_gap`.
    let single_window_rect =
      self.workspace_rect_with_gap_config(single_gap)?;

    Ok(multi_window_rect.union(&single_window_rect))
  }

  pub fn to_dto(&self) -> anyhow::Result<ContainerDto> {
    let rect = self.to_rect()?;
    let config = self.config();

    let children = self
      .children()
      .iter()
      .map(CommonGetters::to_dto)
      .try_collect()?;

    Ok(ContainerDto::Workspace(WorkspaceDto {
      id: self.id(),
      name: config.name,
      display_name: config.display_name,
      parent_id: self.parent().map(|parent| parent.id()),
      children,
      child_focus_order: self.0.borrow().child_focus_order.clone().into(),
      has_focus: self.has_focus(None),
      is_displayed: self.is_displayed(),
      width: rect.width(),
      height: rect.height(),
      x: rect.x(),
      y: rect.y(),
      tiling_direction: self.tiling_direction(),
      layout: self.layout(),
    }))
  }
}

impl_container_debug!(Workspace);
impl_common_getters!(Workspace);
impl_tiling_direction_getters!(Workspace);

impl PositionGetters for Workspace {
  fn to_rect(&self) -> anyhow::Result<Rect> {
    self.workspace_rect_with_gap_config(&self.outer_gaps())
  }
}

impl std::fmt::Display for Workspace {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "Workspace(name={}, tiling_direction={:?}, layout={:?})",
      self.config().name,
      self.tiling_direction(),
      self.layout(),
    )
  }
}

#[cfg(test)]
mod tests {
  use wm_common::{TilingDirection, WorkspaceLayout};

  use super::Workspace;
  use crate::{
    commands::container::{attach_container, detach_container},
    models::{Container, Monitor, TilingWindow, WindowContainer},
    traits::{PositionGetters, TilingSizeGetters},
  };

  fn scrolling_fixture() -> (
    Workspace,
    TilingWindow,
    TilingWindow,
    TilingWindow,
  ) {
    let window_1 = TilingWindow::mock().call();
    let window_2 = TilingWindow::mock().call();
    let window_3 = TilingWindow::mock().call();

    let workspace = Workspace::mock()
      .layout(WorkspaceLayout::Scrolling)
      .tiling_containers(vec![
        window_1.clone().into(),
        window_2.clone().into(),
        window_3.clone().into(),
      ])
      .call();

    Monitor::mock().workspaces(vec![workspace.clone()]).call();

    (workspace, window_1, window_2, window_3)
  }

  #[test]
  fn scrolling_columns_keep_stable_widths() {
    let (workspace, window_1, window_2, window_3) = scrolling_fixture();

    // New columns get a default width without resizing siblings.
    assert_eq!(window_1.tiling_size(), 1.0);
    assert_eq!(window_2.tiling_size(), 0.5);
    assert_eq!(window_3.tiling_size(), 0.5);

    // Viewport is 1680px wide, so columns are 1680/840/840px.
    assert_eq!(window_1.to_rect().unwrap().width(), 1680);
    assert_eq!(window_2.to_rect().unwrap().width(), 840);
    assert_eq!(window_3.to_rect().unwrap().width(), 840);

    assert_eq!(window_1.to_rect().unwrap().x(), 0);
    assert_eq!(window_2.to_rect().unwrap().x(), 1680);
    assert_eq!(window_3.to_rect().unwrap().x(), 2520);

    assert_eq!(workspace.max_scroll_offset().unwrap(), 1680);
  }

  #[test]
  fn ensure_visible_scrolls_to_focused_column() {
    let (workspace, window_1, window_2, window_3) = scrolling_fixture();

    let container_3: Container = window_3.clone().into();
    assert!(workspace.ensure_visible(&container_3).unwrap());
    assert_eq!(workspace.scroll_offset(), 1680);

    // Third column is now fully inside the viewport.
    let rect_3 = window_3.to_rect().unwrap();
    assert_eq!((rect_3.x(), rect_3.width()), (840, 840));

    let container_2: Container = window_2.clone().into();

    // Second column is already visible at this offset.
    assert!(!workspace.ensure_visible(&container_2).unwrap());
    assert_eq!(workspace.scroll_offset(), 1680);

    let container_1: Container = window_1.clone().into();
    assert!(workspace.ensure_visible(&container_1).unwrap());
    assert_eq!(workspace.scroll_offset(), 0);

    // Scrolls the minimum amount to show the second column.
    assert!(workspace.ensure_visible(&container_2).unwrap());
    assert_eq!(workspace.scroll_offset(), 840);

    // Already-visible columns don't change the offset.
    assert!(!workspace.ensure_visible(&container_2).unwrap());
    assert_eq!(workspace.scroll_offset(), 840);
  }

  #[test]
  fn scrolled_out_columns_are_detected() {
    let (workspace, window_1, _, window_3) = scrolling_fixture();

    let window_1: WindowContainer = window_1.clone().into();
    let window_3: WindowContainer = window_3.clone().into();

    assert!(!workspace.is_window_scrolled_out(&window_1));
    assert!(workspace.is_window_scrolled_out(&window_3));

    let container_3: Container = window_3.clone().into();
    workspace.ensure_visible(&container_3).unwrap();

    assert!(workspace.is_window_scrolled_out(&window_1));
    assert!(!workspace.is_window_scrolled_out(&window_3));
  }

  #[test]
  fn detach_keeps_sibling_widths_in_scrolling() {
    let (workspace, window_1, window_2, _) = scrolling_fixture();

    detach_container(window_1.clone().into()).unwrap();

    assert_eq!(window_2.tiling_size(), 0.5);
    assert_eq!(workspace.max_scroll_offset().unwrap(), 0);
    assert_eq!(workspace.scroll_offset(), 0);
  }

  #[test]
  fn attach_keeps_sibling_widths_in_scrolling() {
    let (workspace, _, window_2, _) = scrolling_fixture();

    let window_4 = TilingWindow::mock().call();
    let container_4: Container = window_4.clone().into();
    attach_container(
      &container_4,
      &workspace.clone().into(),
      None,
    )
    .unwrap();

    assert_eq!(window_4.tiling_size(), 0.5);
    assert_eq!(window_2.tiling_size(), 0.5);
  }

  #[test]
  fn scrolling_forces_horizontal_direction() {
    let workspace = Workspace::mock()
      .tiling_direction(TilingDirection::Vertical)
      .layout(WorkspaceLayout::Scrolling)
      .call();

    assert_eq!(
      workspace.tiling_direction(),
      TilingDirection::Horizontal
    );
  }

  #[test]
  fn set_layout_resets_scroll_offset() {
    let (workspace, _, _, window_3) = scrolling_fixture();

    let container_3: Container = window_3.clone().into();
    workspace.ensure_visible(&container_3).unwrap();
    assert_eq!(workspace.scroll_offset(), 1680);

    workspace.set_layout(WorkspaceLayout::Tiling);
    assert_eq!(workspace.scroll_offset(), 0);

    workspace.set_layout(WorkspaceLayout::Scrolling);
    assert_eq!(
      workspace.tiling_direction(),
      TilingDirection::Horizontal
    );
  }
}
