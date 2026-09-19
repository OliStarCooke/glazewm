use std::{
  cell::{Ref, RefCell, RefMut},
  collections::VecDeque,
  rc::Rc,
  time::{Duration, Instant},
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  CenterFocusedColumn, ContainerDto, GapsConfig, ScrollingConfig,
  TilingDirection, WorkspaceConfig, WorkspaceDto, WorkspaceLayout,
};
use wm_platform::{Direction, Rect, RectDelta};

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
  scrolling_config: ScrollingConfig,
  tiling_direction: TilingDirection,
  layout: WorkspaceLayout,
  scroll_offset: i32,
  scroll_target: i32,
  scroll_anim_from: i32,
  scroll_anim_start: Option<Instant>,
  scroll_anim_duration: Duration,
}

impl Workspace {
  pub fn new(
    config: WorkspaceConfig,
    gaps_config: GapsConfig,
    scrolling_config: ScrollingConfig,
    tiling_direction: TilingDirection,
  ) -> Self {
    // Scrolling workspaces always use a horizontal strip of columns.
    let tiling_direction = match config.layout {
      WorkspaceLayout::Scrolling => TilingDirection::Horizontal,
      WorkspaceLayout::Tiling => tiling_direction,
    };

    let anim_duration = Duration::from_millis(
      scrolling_config.animation_duration_ms.max(1),
    );

    let workspace = WorkspaceInner {
      id: Uuid::new_v4(),
      parent: None,
      children: VecDeque::new(),
      child_focus_order: VecDeque::new(),
      layout: config.layout.clone(),
      scroll_offset: 0,
      scroll_target: 0,
      scroll_anim_from: 0,
      scroll_anim_start: None,
      scroll_anim_duration: anim_duration,
      config,
      gaps_config,
      scrolling_config,
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

    let mut inner = self.0.borrow_mut();
    inner.layout = layout;
    inner.scroll_offset = 0;
    inner.scroll_target = 0;
    inner.scroll_anim_from = 0;
    inner.scroll_anim_start = None;
  }

  /// Scrolling options for the workspace.
  pub fn scrolling_config(&self) -> ScrollingConfig {
    self.0.borrow().scrolling_config.clone()
  }

  /// Updates the scrolling options for the workspace.
  ///
  /// Re-clamps the scroll position and snaps any in-progress animation
  /// when animations are disabled.
  pub fn set_scrolling_config(&self, config: ScrollingConfig) {
    let mut inner = self.0.borrow_mut();
    inner.scroll_anim_duration =
      Duration::from_millis(config.animation_duration_ms.max(1));
    inner.scrolling_config = config.clone();
    drop(inner);

    // Snap to target when animations get disabled mid-flight.
    if !config.animation_enabled && self.is_scroll_animating() {
      let target = self.scroll_target();
      let mut inner = self.0.borrow_mut();
      inner.scroll_offset = target;
      inner.scroll_anim_start = None;
    }

    // Re-clamp a stale position (e.g. after viewport resize).
    let clamped = self.scroll_offset();
    let clamped_target = self.scroll_target();
    let mut inner = self.0.borrow_mut();
    inner.scroll_offset = clamped;
    inner.scroll_target = clamped_target;
    if inner.scroll_offset == inner.scroll_target {
      inner.scroll_anim_start = None;
    }
  }

  /// Horizontal scroll offset of the viewport in pixels.
  ///
  /// Clamped to the valid range, so stale offsets (e.g. after a column
  /// is removed or the monitor is resized) never affect layout. The
  /// range allows negative values so edge columns can be centered.
  pub fn scroll_offset(&self) -> i32 {
    let offset = self.0.borrow().scroll_offset;
    self
      .scroll_bounds()
      .map(|(min, max)| offset.clamp(min, max))
      .unwrap_or(offset)
  }

  /// Desired scroll offset once any running animation completes.
  pub fn scroll_target(&self) -> i32 {
    let target = self.0.borrow().scroll_target;
    self
      .scroll_bounds()
      .map(|(min, max)| target.clamp(min, max))
      .unwrap_or(target)
  }

  /// Sets the scroll offset instantly, cancelling any animation.
  pub fn set_scroll_offset(&self, offset: i32) {
    let clamped = self
      .scroll_bounds()
      .map(|(min, max)| offset.clamp(min, max))
      .unwrap_or(offset);

    let mut inner = self.0.borrow_mut();
    inner.scroll_offset = clamped;
    inner.scroll_target = clamped;
    inner.scroll_anim_from = clamped;
    inner.scroll_anim_start = None;
  }

  /// Sets the desired scroll offset.
  ///
  /// When animations are enabled, the visible offset animates towards
  /// the target (see `tick_scroll_animation`). Returns whether the
  /// target changed.
  pub fn set_target_scroll_offset(&self, target: i32) -> bool {
    let clamped = self
      .scroll_bounds()
      .map(|(min, max)| target.clamp(min, max))
      .unwrap_or(target);

    let (stored_target, animating, current) = {
      let inner = self.0.borrow();
      (
        inner.scroll_target,
        inner.scroll_anim_start.is_some(),
        inner.scroll_offset,
      )
    };

    // Clamp the stored target too, so stale viewports (e.g. after a
    // resize) self-heal without restarting the animation.
    let stored_clamped = self
      .scroll_bounds()
      .map(|(min, max)| stored_target.clamp(min, max))
      .unwrap_or(stored_target);

    if clamped == stored_clamped && animating {
      return false;
    }

    if clamped == stored_clamped {
      // Snap a drifted offset without starting an animation.
      if current != clamped {
        let mut inner = self.0.borrow_mut();
        inner.scroll_offset = clamped;
        inner.scroll_target = clamped;
        inner.scroll_anim_from = clamped;
        inner.scroll_anim_start = None;
        return true;
      }
      return false;
    }

    if !self.0.borrow().scrolling_config.animation_enabled {
      let mut inner = self.0.borrow_mut();
      inner.scroll_offset = clamped;
      inner.scroll_target = clamped;
      inner.scroll_anim_from = clamped;
      inner.scroll_anim_start = None;
      return true;
    }

    let from = self.scroll_offset();
    if from == clamped {
      let mut inner = self.0.borrow_mut();
      inner.scroll_offset = clamped;
      inner.scroll_target = clamped;
      inner.scroll_anim_from = clamped;
      inner.scroll_anim_start = None;
      return true;
    }

    let duration = self.0.borrow().scroll_anim_duration;
    let mut inner = self.0.borrow_mut();
    inner.scroll_target = clamped;
    inner.scroll_anim_from = from;
    inner.scroll_anim_start = Some(Instant::now());
    inner.scroll_anim_duration = duration;
    true
  }

  /// Whether a scroll animation is currently running.
  pub fn is_scroll_animating(&self) -> bool {
    self.0.borrow().scroll_anim_start.is_some()
  }

  /// Advances a running scroll animation towards its target.
  ///
  /// Uses an ease-out-cubic curve over the configured duration.
  /// Returns whether the visible offset changed.
  pub fn tick_scroll_animation(&self, now: Instant) -> bool {
    let (from, target, start, duration, current) = {
      let inner = self.0.borrow();
      (
        inner.scroll_anim_from,
        inner.scroll_target,
        inner.scroll_anim_start,
        inner.scroll_anim_duration,
        inner.scroll_offset,
      )
    };

    let Some(start) = start else {
      return false;
    };

    // Clamp the target in case the viewport changed mid-animation.
    let target = self
      .scroll_bounds()
      .map(|(min, max)| target.clamp(min, max))
      .unwrap_or(target);

    let elapsed = now.saturating_duration_since(start);
    let total = duration.as_secs_f32().max(0.001);
    let progress = (elapsed.as_secs_f32() / total).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - progress).powi(3);

    #[allow(clippy::cast_possible_truncation)]
    let interpolated =
      (from as f32 + (target - from) as f32 * eased).round() as i32;

    let clamped = self
      .scroll_bounds()
      .map(|(min, max)| interpolated.clamp(min, max))
      .unwrap_or(interpolated);

    let finished = progress >= 1.0 || clamped == target;
    let new_offset = if finished { target } else { clamped };
    let changed = new_offset != current;

    let mut inner = self.0.borrow_mut();
    inner.scroll_target = target;
    inner.scroll_offset = new_offset;
    if finished {
      inner.scroll_anim_from = target;
      inner.scroll_anim_start = None;
    }
    changed
  }

  /// Valid scroll range, allowing negative offsets so edge columns can
  /// be centered with empty space around them.
  ///
  /// Returns `(0, 0)` for non-scrolling workspaces.
  pub fn scroll_bounds(&self) -> anyhow::Result<(i32, i32)> {
    if !self.is_scrolling() {
      return Ok((0, 0));
    }

    let viewport = self.to_rect()?;
    let columns = self.tiling_children().collect::<Vec<_>>();
    if columns.is_empty() {
      return Ok((0, 0));
    }

    let total_width = self.total_strip_width()?;
    let base_max = (total_width - viewport.width()).max(0);

    let first_width = self.scrolling_column_width(&columns[0])?;
    let last_width =
      self.scrolling_column_width(&columns[columns.len() - 1])?;

    // Centered offsets for the edge columns. Negative when the column
    // is narrower than the viewport (empty space on both sides).
    let first_centered = -(viewport.width() - first_width) / 2;
    let last_centered =
      (total_width - last_width) - (viewport.width() - last_width) / 2;

    Ok((0.min(first_centered), base_max.max(last_centered)))
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

  /// Scroll offset that centers the given column in the viewport.
  ///
  /// The result may lie outside `max_scroll_offset` so edge columns
  /// can be centered with empty space around them. Callers should
  /// apply it via `set_target_scroll_offset`, which clamps to
  /// `scroll_bounds`.
  pub fn centered_offset_for(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<i32> {
    let viewport = self.to_rect()?;
    let unscrolled_x = self.column_unscrolled_x(column)?;
    let width = self.scrolling_column_width(column)?;
    Ok(unscrolled_x - viewport.x() - (viewport.width() - width) / 2)
  }

  /// Edge-aligned offset that fully shows the column, or `None` when
  /// the column is already fully visible at the current target.
  fn edge_offset_for(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<Option<i32>> {
    let viewport = self.to_rect()?;
    let unscrolled_x = self.column_unscrolled_x(column)?;
    let width = self.scrolling_column_width(column)?;
    let target = self.scroll_target();

    let x = unscrolled_x - target;
    if x < viewport.x() {
      Ok(Some(unscrolled_x - viewport.x()))
    } else if x + width > viewport.x() + viewport.width() {
      Ok(Some(
        unscrolled_x + width - (viewport.x() + viewport.width()),
      ))
    } else {
      Ok(None)
    }
  }

  /// Whether centering is needed under `on-overflow` mode.
  ///
  /// Returns true when the focused column does not fit on screen
  /// together with its largest adjacent column.
  fn needs_overflow_centering(
    &self,
    column: &TilingContainer,
  ) -> anyhow::Result<bool> {
    let viewport = self.to_rect()?;
    let width = self.scrolling_column_width(column)?;
    let gap = self.scrolling_inner_gap()?;

    let mut adjacent_width = 0;
    // Find neighbors by index within the workspace children.
    let children = self.tiling_children().collect::<Vec<_>>();
    if let Some(index) =
      children.iter().position(|child| child.id() == column.id())
    {
      if let Some(prev) = index.checked_sub(1).and_then(|i| children.get(i))
      {
        adjacent_width =
          adjacent_width.max(self.scrolling_column_width(prev)?);
      }
      if let Some(next) = children.get(index + 1) {
        adjacent_width =
          adjacent_width.max(self.scrolling_column_width(next)?);
      }
    }

    Ok(width + adjacent_width + gap > viewport.width())
  }

  /// Scrolls the viewport to show the column containing the container.
  ///
  /// Honors `center_focused_column` and `always_center_single_column`.
  /// Visibility is evaluated at the current target so retargeting
  /// mid-animation chains smoothly. Returns whether the target
  /// changed.
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

    let scrolling_config = self.scrolling_config();
    let is_single_column = self.tiling_children().nth(1).is_none();

    if is_single_column && scrolling_config.always_center_single_column {
      return Ok(
        self.set_target_scroll_offset(self.centered_offset_for(&column)?),
      );
    }

    match scrolling_config.center_focused_column {
      CenterFocusedColumn::Always => Ok(
        self.set_target_scroll_offset(self.centered_offset_for(&column)?),
      ),
      CenterFocusedColumn::OnOverflow => {
        match self.edge_offset_for(&column)? {
          None => Ok(false),
          Some(edge_target) => {
            if self.needs_overflow_centering(&column)? {
              Ok(self
                .set_target_scroll_offset(self.centered_offset_for(&column)?))
            } else {
              Ok(self.set_target_scroll_offset(edge_target))
            }
          }
        }
      }
      CenterFocusedColumn::Never => match self.edge_offset_for(&column)? {
        None => Ok(false),
        Some(edge_target) => Ok(self.set_target_scroll_offset(edge_target)),
      },
    }
  }

  /// Centers the column containing the container in the viewport.
  ///
  /// Returns whether the target changed.
  pub fn center_column(
    &self,
    container: &Container,
  ) -> anyhow::Result<bool> {
    if !self.is_scrolling() {
      return Ok(false);
    }

    let Some(column) = self.focus_column(container) else {
      return Ok(false);
    };

    Ok(self.set_target_scroll_offset(self.centered_offset_for(&column)?))
  }

  /// Pans the viewport one column without changing focus.
  ///
  /// Right increases the offset (shows later columns); left decreases
  /// it. Columns wider than the viewport align to their leading edge.
  /// Returns whether the target changed.
  pub fn scroll_view(&self, direction: &Direction) -> anyhow::Result<bool> {
    if !self.is_scrolling() {
      return Ok(false);
    }

    let (viewport_left, viewport_width) = {
      let viewport = self.to_rect()?;
      (viewport.x(), viewport.width())
    };

    let columns = self.tiling_children().collect::<Vec<_>>();
    if columns.is_empty() {
      return Ok(false);
    }

    let base = self.scroll_target();
    let viewport_right = viewport_left + viewport_width;

    let new_target = match direction {
      Direction::Right => {
        let mut target = None;
        for column in &columns {
          let unscrolled = self.column_unscrolled_x(column)?;
          let width = self.scrolling_column_width(column)?;
          // First column extending past the right edge.
          if unscrolled + width - base > viewport_right {
            target = Some(unscrolled - viewport_left);
            break;
          }
        }
        target
      }
      Direction::Left => {
        let mut target = None;
        for column in columns.iter().rev() {
          let unscrolled = self.column_unscrolled_x(column)?;
          let width = self.scrolling_column_width(column)?;
          // Last column extending past the left edge.
          if unscrolled - base < viewport_left {
            target =
              Some(unscrolled + width - (viewport_left + viewport_width));
            break;
          }
        }
        target
      }
      Direction::Up | Direction::Down => None,
    };

    match new_target {
      Some(new_target) => Ok(self.set_target_scroll_offset(new_target)),
      None => Ok(false),
    }
  }

  /// Cycles the column width through the configured presets.
  ///
  /// Starts from the preset closest to the current width. Returns
  /// whether the width changed.
  pub fn cycle_column_preset(
    &self,
    column: &TilingContainer,
    back: bool,
  ) -> bool {
    let presets = self.scrolling_config().normalized_presets();
    if presets.is_empty() {
      return false;
    }

    let current = column.tiling_size();
    let mut closest_index = 0;
    let mut closest_distance = f32::MAX;
    for (index, preset) in presets.iter().enumerate() {
      let distance = (preset - current).abs();
      if distance < closest_distance {
        closest_distance = distance;
        closest_index = index;
      }
    }

    let next_index = if back {
      closest_index.checked_sub(1).unwrap_or(presets.len() - 1)
    } else {
      (closest_index + 1) % presets.len()
    };

    let next = presets[next_index];
    if (next - current).abs() < f32::EPSILON {
      return false;
    }

    column.set_tiling_size(next);
    true
  }

  /// Toggles the column between full viewport width and the default.
  ///
  /// Returns whether the width changed.
  pub fn toggle_column_maximized(&self, column: &TilingContainer) -> bool {
    let current = column.tiling_size();
    let next = if current >= 0.99 {
      self.scrolling_config().normalized_default_width()
    } else {
      1.0
    };

    if (next - current).abs() < f32::EPSILON {
      return false;
    }

    column.set_tiling_size(next);
    true
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
      scroll_offset: self.scroll_offset(),
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
  use std::time::{Duration, Instant};

  use wm_common::{
    CenterFocusedColumn, ScrollingConfig, TilingDirection, WorkspaceLayout,
  };
  use wm_platform::Direction;

  use super::Workspace;
  use crate::{
    commands::container::{attach_container, detach_container},
    models::{Container, Monitor, TilingWindow, WindowContainer},
    traits::{PositionGetters, TilingDirectionGetters, TilingSizeGetters},
  };

  fn disabled_animation_config() -> ScrollingConfig {
    ScrollingConfig {
      animation_enabled: false,
      ..Default::default()
    }
  }

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
      .scrolling_config(disabled_animation_config())
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

  #[test]
  fn scroll_bounds_allow_centering_edge_columns() {
    let (workspace, _, _, _) = scrolling_fixture();

    // Centering the last 840px column needs offset 2100, past the
    // edge-aligned max of 1680.
    assert_eq!(workspace.scroll_bounds().unwrap(), (0, 2100));
  }

  #[test]
  fn always_center_mode_centers_focused_column() {
    let (workspace, _, _, window_3) = scrolling_fixture();
    workspace.set_scrolling_config(ScrollingConfig {
      center_focused_column: CenterFocusedColumn::Always,
      ..disabled_animation_config()
    });

    let container_3: Container = window_3.clone().into();
    assert!(workspace.ensure_visible(&container_3).unwrap());
    assert_eq!(workspace.scroll_target(), 2100);
    assert_eq!(workspace.scroll_offset(), 2100);

    // Centered 840px column sits at x=420 in the 1680px viewport.
    let rect = window_3.to_rect().unwrap();
    assert_eq!((rect.x(), rect.width()), (420, 840));
  }

  #[test]
  fn always_center_single_column_uses_negative_offset() {
    let window = TilingWindow::mock().call();
    let workspace = Workspace::mock()
      .layout(WorkspaceLayout::Scrolling)
      .scrolling_config(ScrollingConfig {
        always_center_single_column: true,
        ..disabled_animation_config()
      })
      .tiling_containers(vec![window.clone().into()])
      .call();
    Monitor::mock().workspaces(vec![workspace.clone()]).call();

    // Lone columns attach at full width; narrow it to exercise
    // centering with empty space on both sides.
    window.set_tiling_size(0.5);

    let container: Container = window.clone().into();
    assert!(workspace.ensure_visible(&container).unwrap());
    assert_eq!(workspace.scroll_offset(), -420);
    assert_eq!(window.to_rect().unwrap().x(), 420);
  }

  #[test]
  fn on_overflow_centers_only_when_columns_do_not_fit() {
    let (workspace, _, window_2, _) = scrolling_fixture();
    workspace.set_scrolling_config(ScrollingConfig {
      center_focused_column: CenterFocusedColumn::OnOverflow,
      ..disabled_animation_config()
    });

    // 840px column next to a 1680px column overflows together.
    let container_2: Container = window_2.clone().into();
    workspace.set_scroll_offset(0);
    assert!(workspace.ensure_visible(&container_2).unwrap());
    assert_eq!(workspace.scroll_offset(), 1260);

    // Narrow columns that fit together use edge alignment instead.
    let narrow_1 = TilingWindow::mock().call();
    let narrow_2 = TilingWindow::mock().call();
    let narrow_3 = TilingWindow::mock().call();
    let workspace = Workspace::mock()
      .layout(WorkspaceLayout::Scrolling)
      .scrolling_config(ScrollingConfig {
        center_focused_column: CenterFocusedColumn::OnOverflow,
        ..disabled_animation_config()
      })
      .tiling_containers(vec![
        narrow_1.clone().into(),
        narrow_2.clone().into(),
        narrow_3.clone().into(),
      ])
      .call();
    Monitor::mock().workspaces(vec![workspace.clone()]).call();

    for narrow in [&narrow_1, &narrow_2, &narrow_3] {
      narrow.set_tiling_size(0.25);
    }

    let middle: Container = narrow_2.clone().into();
    assert!(!workspace.ensure_visible(&middle).unwrap());
    assert_eq!(workspace.scroll_offset(), 0);
  }

  #[test]
  fn scroll_view_pans_by_column_without_focus_change() {
    let (workspace, _, _, _) = scrolling_fixture();

    assert!(workspace.scroll_view(&Direction::Right).unwrap());
    assert_eq!(workspace.scroll_target(), 1680);

    // At the right edge there is no further column to show.
    assert!(!workspace.scroll_view(&Direction::Right).unwrap());

    assert!(workspace.scroll_view(&Direction::Left).unwrap());
    assert_eq!(workspace.scroll_target(), 0);
  }

  #[test]
  fn preset_cycling_and_maximize_update_widths() {
    let (workspace, _, window_2, _) = scrolling_fixture();
    let column = workspace
      .focus_column(&window_2.clone().into())
      .expect("column");

    assert!(workspace.cycle_column_preset(&column, false));
    assert!((column.tiling_size() - 2.0 / 3.0).abs() < 1e-6);

    assert!(workspace.cycle_column_preset(&column, true));
    assert!((column.tiling_size() - 0.5).abs() < 1e-6);

    assert!(workspace.toggle_column_maximized(&column));
    assert_eq!(column.tiling_size(), 1.0);

    assert!(workspace.toggle_column_maximized(&column));
    assert_eq!(column.tiling_size(), 0.5);
  }

  #[test]
  fn scroll_animation_eases_towards_target() {
    let window_1 = TilingWindow::mock().call();
    let window_2 = TilingWindow::mock().call();
    let window_3 = TilingWindow::mock().call();

    let workspace = Workspace::mock()
      .layout(WorkspaceLayout::Scrolling)
      .scrolling_config(ScrollingConfig::default())
      .tiling_containers(vec![
        window_1.clone().into(),
        window_2.clone().into(),
        window_3.clone().into(),
      ])
      .call();
    Monitor::mock().workspaces(vec![workspace.clone()]).call();

    assert!(workspace.scrolling_config().animation_enabled);

    let container_3: Container = window_3.clone().into();
    assert!(workspace.ensure_visible(&container_3).unwrap());

    // Target is set instantly, visible offset animates towards it.
    assert_eq!(workspace.scroll_target(), 1680);
    assert_eq!(workspace.scroll_offset(), 0);
    assert!(workspace.is_scroll_animating());

    // Mid-animation progress is eased forward (ease-out-cubic jumps
    // quickly towards the target).
    let start = Instant::now();
    workspace.tick_scroll_animation(start + Duration::from_millis(125));
    let offset = workspace.scroll_offset();
    assert!(offset > 0 && offset <= 1680);

    // Far-future tick completes the animation.
    workspace.tick_scroll_animation(start + Duration::from_secs(5));
    assert_eq!(workspace.scroll_offset(), 1680);
    assert!(!workspace.is_scroll_animating());
  }
}
