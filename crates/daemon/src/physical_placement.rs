//! Physical presentation for tiled HWNDs at shared monitor edges.
//!
//! Logical layout (requested widths, membership, scroll) stays unchanged.
//! Applications retain that full geometry, including where it bleeds onto an
//! exactly adjacent monitor. Decorations use the shared projection below to
//! remain clipped at protected owner edges; applications are parked only when
//! no positive owner-visible slice remains.

use crate::state::*;
use leopardwm_core_layout::{Rect, Visibility, WindowPlacement};
use leopardwm_platform_win32::{MonitorId, MonitorInfo, PlacementLanding};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ConstrainedAxes {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl ConstrainedAxes {
    fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhysicalDecision {
    Unchanged,
    Constrained { rect: Rect },
    Parked { rect: Rect },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhysicalKind {
    Unchanged,
    Parked,
}

#[derive(Debug, Clone)]
pub(crate) struct PhysicalPresentation {
    pub physical: WindowPlacement,
    pub kind: PhysicalKind,
    pub request_id: u64,
    pub invalidation_id: u64,
    pub confirmed: bool,
}

fn i32_sat(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn edge_end(origin: i32, extent: i32) -> i64 {
    origin as i64 + extent as i64
}

fn overlap_len(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    a1.min(b1).saturating_sub(a0.max(b0)).max(0)
}

fn rects_equal(a: Rect, b: Rect) -> bool {
    a.x == b.x && a.y == b.y && a.width == b.width && a.height == b.height
}

fn rects_overlap_positive(a: Rect, b: Rect) -> bool {
    overlap_len(
        a.x as i64,
        edge_end(a.x, a.width),
        b.x as i64,
        edge_end(b.x, b.width),
    ) > 0
        && overlap_len(
            a.y as i64,
            edge_end(a.y, a.height),
            b.y as i64,
            edge_end(b.y, b.height),
        ) > 0
}

#[derive(Clone, Copy)]
enum SharedEdge {
    Left,
    Right,
    Top,
    Bottom,
}

fn exact_touching_edge(owner: Rect, neighbor: Rect) -> Option<SharedEdge> {
    let ox0 = owner.x as i64;
    let ox1 = edge_end(owner.x, owner.width);
    let oy0 = owner.y as i64;
    let oy1 = edge_end(owner.y, owner.height);
    let nx0 = neighbor.x as i64;
    let nx1 = edge_end(neighbor.x, neighbor.width);
    let ny0 = neighbor.y as i64;
    let ny1 = edge_end(neighbor.y, neighbor.height);
    let y_span = overlap_len(oy0, oy1, ny0, ny1);
    let x_span = overlap_len(ox0, ox1, nx0, nx1);
    if nx0 == ox1 && y_span > 0 {
        Some(SharedEdge::Right)
    } else if nx1 == ox0 && y_span > 0 {
        Some(SharedEdge::Left)
    } else if ny0 == oy1 && x_span > 0 {
        Some(SharedEdge::Bottom)
    } else if ny1 == oy0 && x_span > 0 {
        Some(SharedEdge::Top)
    } else {
        None
    }
}

pub(crate) fn topology_signature(
    monitors: &HashMap<MonitorId, MonitorInfo>,
) -> Vec<(MonitorId, i32, i32, i32, i32, u32)> {
    let mut signature: Vec<_> = monitors
        .iter()
        .map(|(id, monitor)| {
            (
                *id,
                monitor.rect.x,
                monitor.rect.y,
                monitor.rect.width,
                monitor.rect.height,
                (monitor.scale_factor * 1000.0).round() as u32,
            )
        })
        .collect();
    signature.sort_unstable();
    signature
}

pub(crate) fn monitor_rects(monitors: &HashMap<MonitorId, MonitorInfo>) -> Vec<Rect> {
    monitors.values().map(|monitor| monitor.rect).collect()
}

/// Union of two rectangles using widened edge arithmetic.
pub(crate) fn union_rects(a: Rect, b: Rect) -> Rect {
    let x0 = (a.x as i64).min(b.x as i64);
    let y0 = (a.y as i64).min(b.y as i64);
    let x1 = edge_end(a.x, a.width).max(edge_end(b.x, b.width));
    let y1 = edge_end(a.y, a.height).max(edge_end(b.y, b.height));
    Rect::new(i32_sat(x0), i32_sat(y0), i32_sat(x1 - x0), i32_sat(y1 - y0))
}

pub(crate) fn tab_strip_rect(window: Rect, strip_height: i32, bottom_gap: i32) -> Rect {
    Rect::new(
        window.x,
        window
            .y
            .saturating_sub(strip_height.max(0))
            .saturating_sub(bottom_gap.max(0)),
        window.width,
        strip_height.max(0),
    )
}

pub(crate) fn expand_border_overlay(content: Rect, border_width: u32, outside: bool) -> Rect {
    let bw = border_width as i32;
    if outside {
        let rx = content.x.saturating_add(1);
        let ry = content.y.saturating_add(1);
        let tw = (content.width - 2).max(0);
        let th = (content.height - 2).max(0);
        Rect::new(
            rx.saturating_sub(bw),
            ry.saturating_sub(bw),
            tw.saturating_add(2 * bw),
            th.saturating_add(2 * bw),
        )
    } else {
        content
    }
}

/// Project `window` onto `owner` only at exactly-touching neighbor edges.
///
/// Always derived from the original rectangle; never from a previous clip.
pub(crate) fn project_physical_rect(
    window: Rect,
    owner: Rect,
    monitor_rects: &[Rect],
) -> PhysicalDecision {
    if window.width <= 0 || window.height <= 0 {
        return PhysicalDecision::Unchanged;
    }

    let mut left = window.x as i64;
    let mut right = edge_end(window.x, window.width);
    let mut top = window.y as i64;
    let mut bottom = edge_end(window.y, window.height);
    let mut axes = ConstrainedAxes::default();

    for neighbor in monitor_rects {
        if rects_equal(*neighbor, owner) {
            continue;
        }
        let Some(edge) = exact_touching_edge(owner, *neighbor) else {
            continue;
        };
        if !rects_overlap_positive(window, *neighbor) {
            continue;
        }
        match edge {
            SharedEdge::Right => {
                let owner_right = edge_end(owner.x, owner.width);
                if right > owner_right {
                    right = owner_right;
                    axes.right = true;
                }
            }
            SharedEdge::Left => {
                let owner_left = owner.x as i64;
                if left < owner_left {
                    left = owner_left;
                    axes.left = true;
                }
            }
            SharedEdge::Bottom => {
                let owner_bottom = edge_end(owner.y, owner.height);
                if bottom > owner_bottom {
                    bottom = owner_bottom;
                    axes.bottom = true;
                }
            }
            SharedEdge::Top => {
                let owner_top = owner.y as i64;
                if top < owner_top {
                    top = owner_top;
                    axes.top = true;
                }
            }
        }
    }

    if !axes.any() {
        return PhysicalDecision::Unchanged;
    }

    let width = right - left;
    let height = bottom - top;
    if width <= 0 || height <= 0 {
        return PhysicalDecision::Parked {
            rect: offscreen_park_rect(window, monitor_rects, (0, 0, 0, 0)),
        };
    }

    let rect = Rect::new(i32_sat(left), i32_sat(top), i32_sat(width), i32_sat(height));
    if rects_equal(rect, window) {
        PhysicalDecision::Unchanged
    } else {
        PhysicalDecision::Constrained { rect }
    }
}

/// Pick a recoverable frame-space parking location that clears every monitor.
///
/// Both coordinates remain at or beyond the shared MoveOffScreen sentinel, so
/// no-state recovery retains its narrow ownership test. The caller supplies
/// visible-to-frame insets because the native move targets the outer frame.
/// If an extreme virtual-desktop geometry cannot be represented, final native
/// readback leaves the placement unconfirmed instead of treating it as safe.
pub(crate) fn offscreen_park_rect(
    window: Rect,
    monitor_rects: &[Rect],
    insets: (i32, i32, i32, i32),
) -> Rect {
    const SENTINEL: i64 = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD as i64;
    let (_, _, right, bottom) = insets;
    let outer_width = i64::from(window.width.max(1)) + i64::from(right.max(0));
    let outer_height = i64::from(window.height.max(1)) + i64::from(bottom.max(0));
    let x = monitor_rects
        .iter()
        .map(|monitor| i64::from(monitor.x) - outer_width)
        .min()
        .unwrap_or(SENTINEL)
        .min(SENTINEL);
    let y = monitor_rects
        .iter()
        .map(|monitor| i64::from(monitor.y) - outer_height)
        .min()
        .unwrap_or(SENTINEL)
        .min(SENTINEL);
    Rect::new(i32_sat(x), i32_sat(y), window.width, window.height)
}

pub(crate) fn park_offscreen_avoiding_neighbors(
    placements: &mut [WindowPlacement],
    owner_id: MonitorId,
    monitors: &HashMap<MonitorId, MonitorInfo>,
) {
    if !monitors.contains_key(&owner_id) {
        return;
    }
    let rects = monitor_rects(monitors);
    for placement in placements.iter_mut() {
        if placement.visibility == Visibility::Visible {
            continue;
        }
        let bleeds = monitors
            .iter()
            .filter(|(id, _)| **id != owner_id)
            .any(|(_, monitor)| placement.rect.intersects(&monitor.rect));
        if bleeds {
            placement.rect =
                offscreen_park_rect(placement.rect, &rects, native_insets(placement.window_id));
        }
    }
}

fn physical_from_decision(logical: WindowPlacement, decision: PhysicalDecision) -> WindowPlacement {
    match decision {
        PhysicalDecision::Parked { rect } => WindowPlacement {
            rect,
            visibility: Visibility::OffScreenRight,
            ..logical
        },
        PhysicalDecision::Unchanged | PhysicalDecision::Constrained { .. } => logical,
    }
}

pub(crate) fn decide_physical_rect(
    window: Rect,
    owner: Rect,
    monitor_rects: &[Rect],
    native_insets: (i32, i32, i32, i32),
) -> PhysicalDecision {
    match project_physical_rect(window, owner, monitor_rects) {
        PhysicalDecision::Parked { .. } => PhysicalDecision::Parked {
            rect: offscreen_park_rect(window, monitor_rects, native_insets),
        },
        PhysicalDecision::Unchanged | PhysicalDecision::Constrained { .. } => {
            PhysicalDecision::Unchanged
        }
    }
}

pub(crate) fn parking_clears_monitors(actual: Rect, monitor_rects: &[Rect]) -> bool {
    !monitor_rects
        .iter()
        .any(|monitor| actual.intersects(monitor))
}

fn native_insets(window_id: u64) -> (i32, i32, i32, i32) {
    #[cfg(test)]
    {
        let _ = window_id;
        (0, 0, 0, 0)
    }
    #[cfg(not(test))]
    {
        leopardwm_platform_win32::get_window_invisible_insets(window_id)
    }
}

pub(crate) fn landing_origin_drift(landing: &PlacementLanding) -> Option<(i32, i32)> {
    if landing.failed || landing.unreadable {
        return None;
    }
    if landing.requested_visibility != Visibility::Visible {
        return None;
    }
    let actual = landing.actual_visible_rect?;
    let dx = actual.x - landing.requested_rect.x;
    let dy = actual.y - landing.requested_rect.y;
    if dx.abs() > 2 || dy.abs() > 2 {
        Some((dx, dy))
    } else {
        None
    }
}

impl AppState {
    pub(crate) fn bump_physical_invalidation(&self) -> u64 {
        self.physical_invalidation_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn clear_physical_window_state(&mut self, window_id: u64) {
        self.last_physical_presentations.remove(&window_id);
        self.pending_physical_presentations.remove(&window_id);
        self.bump_physical_invalidation();
    }

    fn owner_id_for_window(&self, window_id: u64) -> Option<MonitorId> {
        self.find_window_workspace(window_id)
            .map(|(monitor_id, _)| monitor_id)
            .or_else(|| {
                self.layout_transition.as_ref().and_then(|transition| {
                    transition
                        .exit_provenance
                        .get(&window_id)
                        .map(|provenance| provenance.owner)
                })
            })
    }

    fn native_window_is_maximized(&self, window_id: u64) -> bool {
        #[cfg(test)]
        if let Some(maximized) = self.injected_window_maximized.get(&window_id) {
            return *maximized;
        }
        leopardwm_platform_win32::is_window_maximized(window_id)
    }

    pub(crate) fn exit_projection_eligible(&self, window_id: u64) -> bool {
        if self.is_application_fullscreen(window_id)
            || self.native_window_is_maximized(window_id)
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.is_tiled && drag.hwnd == window_id)
        {
            return false;
        }

        self.find_window_workspace(window_id)
            .and_then(|(monitor_id, ws_idx)| {
                self.workspaces
                    .get(&monitor_id)
                    .and_then(|workspaces| workspaces.get(ws_idx))
            })
            .is_some_and(|workspace| {
                !workspace.is_floating(window_id)
                    && workspace.fullscreen_window_id() != Some(window_id)
            })
    }

    fn is_projection_exempt(&self, placement: &WindowPlacement) -> bool {
        if placement.visibility != Visibility::Visible || placement.column_index == usize::MAX {
            return true;
        }
        if self.is_application_fullscreen(placement.window_id)
            || self.native_window_is_maximized(placement.window_id)
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.is_tiled && drag.hwnd == placement.window_id)
        {
            return true;
        }

        if let Some(provenance) = self
            .layout_transition
            .as_ref()
            .and_then(|transition| transition.exit_provenance.get(&placement.window_id))
        {
            return !provenance.eligible;
        }

        self.find_window_workspace(placement.window_id)
            .and_then(|(monitor_id, ws_idx)| {
                self.workspaces
                    .get(&monitor_id)
                    .and_then(|workspaces| workspaces.get(ws_idx))
            })
            .is_some_and(|workspace| {
                workspace.is_floating(placement.window_id)
                    || workspace.fullscreen_window_id() == Some(placement.window_id)
            })
    }

    fn current_pending_physical_presentation(
        &self,
        window_id: u64,
    ) -> Option<&PhysicalPresentation> {
        let presentation = self.pending_physical_presentations.get(&window_id)?;
        (self.inflight_request_id == Some(presentation.request_id)
            && self.pending_physical_request_id == presentation.request_id
            && self.pending_physical_invalidation_id == presentation.invalidation_id)
            .then_some(presentation)
    }

    pub(crate) fn expected_physical_rect(&self, window_id: u64) -> Option<Rect> {
        self.current_pending_physical_presentation(window_id)
            .or_else(|| self.last_physical_presentations.get(&window_id))
            .map(|presentation| presentation.physical.rect)
    }

    pub(crate) fn is_physically_parked(&self, window_id: u64) -> bool {
        self.current_pending_physical_presentation(window_id)
            .or_else(|| self.last_physical_presentations.get(&window_id))
            .is_some_and(|presentation| presentation.kind == PhysicalKind::Parked)
    }

    pub(crate) fn physical_landing_is_safe_to_expose(&self, window_id: u64) -> bool {
        self.physical_invalidation_id.load(Ordering::SeqCst)
            == self.last_applied_physical_invalidation
            && self
                .last_physical_presentations
                .get(&window_id)
                .is_some_and(|presentation| {
                    presentation.confirmed && presentation.kind != PhysicalKind::Parked
                })
    }

    pub(crate) fn apply_physical_projection(
        &mut self,
        placements: Vec<WindowPlacement>,
    ) -> Vec<WindowPlacement> {
        let request_id = self.physical_request_seq.wrapping_add(1);
        self.physical_request_seq = request_id;
        self.physical_dispatch_request_id
            .store(request_id, Ordering::SeqCst);
        let invalidation_id = self.physical_invalidation_id.load(Ordering::SeqCst);
        self.inflight_request_id = Some(request_id);
        let rects = monitor_rects(&self.monitors);
        let mut presentations = HashMap::new();
        let mut physical = Vec::with_capacity(placements.len());

        for logical in placements {
            let Some(owner_id) = self.owner_id_for_window(logical.window_id) else {
                physical.push(logical);
                continue;
            };
            let Some(owner) = self.monitors.get(&owner_id).map(|monitor| monitor.rect) else {
                physical.push(logical);
                continue;
            };
            let decision = if self.is_projection_exempt(&logical) {
                PhysicalDecision::Unchanged
            } else {
                decide_physical_rect(
                    logical.rect,
                    owner,
                    &rects,
                    native_insets(logical.window_id),
                )
            };
            let physical_placement = physical_from_decision(logical.clone(), decision);
            presentations.insert(
                logical.window_id,
                PhysicalPresentation {
                    physical: physical_placement.clone(),
                    kind: if matches!(decision, PhysicalDecision::Parked { .. }) {
                        PhysicalKind::Parked
                    } else {
                        PhysicalKind::Unchanged
                    },
                    request_id,
                    invalidation_id,
                    confirmed: false,
                },
            );
            physical.push(physical_placement);
        }

        self.pending_physical_presentations = presentations.clone();
        self.inflight_origins.clear();
        self.inflight_origins.insert(request_id, presentations);
        self.pending_physical_request_id = request_id;
        self.pending_physical_invalidation_id = invalidation_id;
        physical
    }

    pub(crate) fn abandon_physical_request(&mut self, request_id: u64, invalidation_id: u64) {
        if request_id == 0
            || self.pending_physical_request_id != request_id
            || self.pending_physical_invalidation_id != invalidation_id
        {
            return;
        }

        if let Some(mut origins) = self.inflight_origins.remove(&request_id) {
            if !origins.is_empty() {
                for presentation in origins.values_mut() {
                    presentation.confirmed = false;
                }
                self.last_physical_presentations = origins;
                self.last_applied_physical_invalidation =
                    self.physical_invalidation_id.load(Ordering::SeqCst);
                self.last_topology_signature = topology_signature(&self.monitors);
            }
        }
        if self.inflight_request_id == Some(request_id) {
            self.inflight_request_id = None;
        }
        self.pending_physical_presentations.clear();
        self.pending_physical_request_id = 0;
        self.pending_physical_invalidation_id = 0;
    }

    pub(crate) fn physical_request_ids(&self) -> (u64, u64) {
        (
            self.pending_physical_request_id,
            self.pending_physical_invalidation_id,
        )
    }

    pub(crate) fn physical_result_matches_inflight(
        &self,
        request_id: u64,
        invalidation_id: u64,
    ) -> bool {
        request_id == self.pending_physical_request_id
            && invalidation_id == self.pending_physical_invalidation_id
            && (request_id == 0 || self.inflight_request_id == Some(request_id))
    }

    pub(crate) fn physical_result_is_current(&self, request_id: u64, invalidation_id: u64) -> bool {
        self.physical_result_matches_inflight(request_id, invalidation_id)
            && invalidation_id == self.physical_invalidation_id.load(Ordering::SeqCst)
    }

    pub(crate) fn allows_core_size_feedback(&self, request_id: u64, window_id: u64) -> bool {
        let Some(origin) = self
            .inflight_origins
            .get(&request_id)
            .and_then(|origins| origins.get(&window_id))
        else {
            return true;
        };

        matches!(origin.kind, PhysicalKind::Unchanged)
    }

    pub(crate) fn physical_fast_path_ok(&self) -> bool {
        self.physical_invalidation_id.load(Ordering::SeqCst)
            == self.last_applied_physical_invalidation
            && topology_signature(&self.monitors) == self.last_topology_signature
            && self
                .last_physical_presentations
                .values()
                .all(|presentation| presentation.confirmed)
    }

    pub(crate) fn consume_physical_landings(
        &mut self,
        request_id: u64,
        invalidation_id: u64,
        landings: &[PlacementLanding],
    ) {
        if !self.physical_result_is_current(request_id, invalidation_id) {
            debug!(
                "Ignoring stale physical landings request={} invalidation={}",
                request_id, invalidation_id
            );
            return;
        }
        let Some(origins) = self.inflight_origins.remove(&request_id) else {
            return;
        };
        if origins.values().any(|origin| {
            origin.request_id != request_id || origin.invalidation_id != invalidation_id
        }) {
            return;
        }
        if self.inflight_request_id == Some(request_id) {
            self.inflight_request_id = None;
        }

        let rects = monitor_rects(&self.monitors);
        let landings_by_id: HashMap<u64, &PlacementLanding> = landings
            .iter()
            .map(|landing| (landing.window_id, landing))
            .collect();
        let mut confirmed = origins.clone();

        for (window_id, presentation) in &origins {
            let Some(entry) = confirmed.get_mut(window_id) else {
                continue;
            };
            let Some(landing) = landings_by_id.get(window_id) else {
                warn!(
                    "Physical placement of window {} is unconfirmed: no readback",
                    window_id
                );
                continue;
            };
            entry.confirmed = !landing.failed
                && !landing.unreadable
                && match presentation.kind {
                    PhysicalKind::Parked => landing
                        .actual_outer_rect
                        .is_some_and(|rect| parking_clears_monitors(rect, &rects)),
                    PhysicalKind::Unchanged => landing.actual_visible_rect.is_some(),
                };
            if !entry.confirmed {
                warn!(
                    "Physical placement of window {} was blocked (failed={} unreadable={})",
                    window_id, landing.failed, landing.unreadable
                );
            } else if matches!(presentation.kind, PhysicalKind::Unchanged) {
                if let Some((dx, dy)) = landing_origin_drift(landing) {
                    warn!(
                        "Window {} landed {}px,{}px away from its requested origin: requested {:?}, actual visible {:?}",
                        window_id, dx, dy, landing.requested_rect, landing.actual_visible_rect
                    );
                }
            }
        }

        self.last_physical_presentations = confirmed;
        self.last_applied_physical_invalidation =
            self.physical_invalidation_id.load(Ordering::SeqCst);
        self.last_topology_signature = topology_signature(&self.monitors);
        if self.pending_physical_request_id == request_id
            && self.pending_physical_invalidation_id == invalidation_id
        {
            self.pending_physical_presentations.clear();
        }
    }

    pub(crate) fn acknowledge_empty_physical_state(
        &mut self,
        request_id: u64,
        invalidation_id: u64,
        logically_empty: bool,
    ) {
        if !logically_empty
            || request_id == 0
            || !self.physical_result_is_current(request_id, invalidation_id)
            || !self.last_physical_presentations.is_empty()
        {
            return;
        }
        let Some(origins) = self.inflight_origins.get(&request_id) else {
            return;
        };
        if !origins.is_empty() {
            return;
        }
        self.last_applied_physical_invalidation =
            self.physical_invalidation_id.load(Ordering::SeqCst);
        self.last_topology_signature = topology_signature(&self.monitors);
    }

    pub(crate) fn swept_rect_meets_protected_boundary(
        &self,
        window_id: u64,
        start: Rect,
        target: Rect,
    ) -> bool {
        let Some(owner_id) = self.owner_id_for_window(window_id) else {
            return false;
        };
        let Some(owner) = self.monitors.get(&owner_id).map(|monitor| monitor.rect) else {
            return false;
        };
        let rects = monitor_rects(&self.monitors);
        !matches!(
            project_physical_rect(union_rects(start, target), owner, &rects),
            PhysicalDecision::Unchanged
        )
    }

    pub(crate) fn project_decoration_rect(&self, window_id: u64, rect: Rect) -> Option<Rect> {
        if self.is_physically_parked(window_id) {
            return None;
        }
        let owner_id = self.owner_id_for_window(window_id)?;
        let owner = self.monitors.get(&owner_id)?.rect;
        let rects = monitor_rects(&self.monitors);
        match project_physical_rect(rect, owner, &rects) {
            PhysicalDecision::Parked { .. } => None,
            PhysicalDecision::Constrained { rect, .. } => Some(rect),
            PhysicalDecision::Unchanged => Some(rect),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leopardwm_core_layout::WindowPlacement;
    use leopardwm_platform_win32::MonitorInfo;

    fn monitor(id: MonitorId, x: i32, y: i32, w: i32, h: i32) -> MonitorInfo {
        MonitorInfo {
            id,
            rect: Rect::new(x, y, w, h),
            work_area: Rect::new(x, y, w, h),
            is_primary: false,
            device_name: String::new(),
            scale_factor: 1.0,
        }
    }

    fn placement(wid: u64, rect: Rect, visibility: Visibility) -> WindowPlacement {
        WindowPlacement {
            window_id: wid,
            rect,
            visibility,
            column_index: 0,
        }
    }

    fn owner_5120() -> Rect {
        Rect::new(0, 0, 5120, 1440)
    }

    fn neighbor_800() -> Rect {
        Rect::new(5120, 0, 800, 600)
    }

    fn reproduction_window() -> Rect {
        Rect::new(4320, 10, 1600, 1440)
    }

    #[test]
    fn full_partial_landing_confirms_without_outer_containment() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 5120, 1440), monitor(2, 5120, 0, 800, 600)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(1600))
            .unwrap();
        let logical = placement(100, reproduction_window(), Visibility::Visible);
        let dispatched = state.apply_physical_projection(vec![logical.clone()]);
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].window_id, logical.window_id);
        assert_eq!(dispatched[0].rect, logical.rect);
        assert_eq!(dispatched[0].visibility, logical.visibility);
        let (request_id, invalidation_id) = state.physical_request_ids();
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: dispatched[0].rect,
                requested_visibility: Visibility::Visible,
                actual_visible_rect: Some(dispatched[0].rect),
                actual_outer_rect: Some(dispatched[0].rect),
                failed: false,
                unreadable: false,
            }],
        );
        assert_eq!(
            state.last_physical_presentations[&100].kind,
            PhysicalKind::Unchanged
        );
        assert!(state.last_physical_presentations[&100].confirmed);
        assert!(!state.is_physically_parked(100));
    }

    #[test]
    fn production_projection_preserves_full_partial_progression_and_parks_zero_slice() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![
                monitor(1, 0, 0, 1000, 1000),
                monitor(2, -800, 0, 800, 1000),
                monitor(3, 1000, 0, 800, 1000),
                monitor(4, 0, -600, 1000, 600),
                monitor(5, 0, 1000, 1000, 600),
            ],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(400))
            .unwrap();
        let widths = {
            let workspace = &state.workspaces[&1][0];
            (
                workspace.columns()[0].width(),
                workspace.effective_column_width(&workspace.columns()[0]),
            )
        };

        for (name, rect) in [
            ("right partial progression", Rect::new(700, 100, 400, 400)),
            ("right further partial", Rect::new(800, 100, 400, 400)),
            ("right reversal", Rect::new(700, 100, 400, 400)),
            ("right stopped partial", Rect::new(700, 100, 400, 400)),
            ("recenter", Rect::new(300, 100, 400, 400)),
            ("left partial progression", Rect::new(-100, 100, 400, 400)),
            ("left further partial", Rect::new(-200, 100, 400, 400)),
            ("left reversal", Rect::new(-100, 100, 400, 400)),
            ("left stopped partial", Rect::new(-100, 100, 400, 400)),
            ("top partial", Rect::new(100, -100, 400, 400)),
            ("bottom partial", Rect::new(100, 700, 400, 400)),
        ] {
            let logical = placement(100, rect, Visibility::Visible);
            let physical = state.apply_physical_projection(vec![logical.clone()]);
            assert_eq!(physical.len(), 1, "{name}");
            assert_eq!(physical[0].rect, logical.rect, "{name}");
            assert_eq!(physical[0].visibility, logical.visibility, "{name}");
            assert_eq!(
                state.pending_physical_presentations[&100].kind,
                PhysicalKind::Unchanged,
                "{name}"
            );
            let workspace = &state.workspaces[&1][0];
            assert_eq!(workspace.columns()[0].width(), widths.0, "{name}");
            assert_eq!(
                workspace.effective_column_width(&workspace.columns()[0]),
                widths.1,
                "{name}"
            );
        }

        let zero_slice = placement(100, Rect::new(1000, 100, 400, 400), Visibility::Visible);
        let parked = state.apply_physical_projection(vec![zero_slice.clone()]);
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].rect.width, zero_slice.rect.width);
        assert_eq!(parked[0].rect.height, zero_slice.rect.height);
        assert_eq!(parked[0].visibility, Visibility::OffScreenRight);
        assert_eq!(
            state.pending_physical_presentations[&100].kind,
            PhysicalKind::Parked
        );
        let workspace = &state.workspaces[&1][0];
        assert_eq!(workspace.columns()[0].width(), widths.0);
        assert_eq!(
            workspace.effective_column_width(&workspace.columns()[0]),
            widths.1
        );
    }

    #[test]
    fn production_projection_preserves_negative_no_neighbor_and_gapped_geometry() {
        let negative_owner = Rect::new(-1920, 0, 1920, 1080);
        let negative_neighbor = Rect::new(0, 0, 2880, 1800);
        let negative_logical = placement(100, Rect::new(-200, 0, 500, 1080), Visibility::Visible);
        let mut negative = AppState::new_with_config(
            crate::config::Config::default(),
            vec![
                monitor(
                    1,
                    negative_owner.x,
                    negative_owner.y,
                    negative_owner.width,
                    negative_owner.height,
                ),
                monitor(
                    2,
                    negative_neighbor.x,
                    negative_neighbor.y,
                    negative_neighbor.width,
                    negative_neighbor.height,
                ),
            ],
        );
        negative.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(500))
            .unwrap();
        let negative_widths = {
            let workspace = &negative.workspaces[&1][0];
            (
                workspace.columns()[0].width(),
                workspace.effective_column_width(&workspace.columns()[0]),
            )
        };
        let negative_physical = negative.apply_physical_projection(vec![negative_logical.clone()]);
        assert_eq!(negative_physical[0].rect, negative_logical.rect);
        assert_eq!(negative_physical[0].visibility, negative_logical.visibility);
        assert_eq!(
            negative.pending_physical_presentations[&100].kind,
            PhysicalKind::Unchanged
        );
        let workspace = &negative.workspaces[&1][0];
        assert_eq!(workspace.columns()[0].width(), negative_widths.0);
        assert_eq!(
            workspace.effective_column_width(&workspace.columns()[0]),
            negative_widths.1
        );

        for (name, monitors) in [
            ("no neighbor", vec![monitor(1, 0, 0, 1000, 1000)]),
            (
                "gapped neighbor",
                vec![monitor(1, 0, 0, 1000, 1000), monitor(2, 1010, 0, 800, 1000)],
            ),
        ] {
            let mut state = AppState::new_with_config(crate::config::Config::default(), monitors);
            state.workspaces.get_mut(&1).unwrap()[0]
                .insert_window(100, Some(400))
                .unwrap();
            let widths = {
                let workspace = &state.workspaces[&1][0];
                (
                    workspace.columns()[0].width(),
                    workspace.effective_column_width(&workspace.columns()[0]),
                )
            };
            let logical = placement(100, Rect::new(800, 100, 400, 400), Visibility::Visible);
            assert_eq!(
                project_physical_rect(
                    logical.rect,
                    state.monitors[&1].rect,
                    &monitor_rects(&state.monitors)
                ),
                PhysicalDecision::Unchanged,
                "{name}"
            );
            let physical = state.apply_physical_projection(vec![logical.clone()]);
            assert_eq!(physical[0].rect, logical.rect, "{name}");
            assert_eq!(physical[0].visibility, logical.visibility, "{name}");
            assert_eq!(
                state.pending_physical_presentations[&100].kind,
                PhysicalKind::Unchanged,
                "{name}"
            );
            let workspace = &state.workspaces[&1][0];
            assert_eq!(workspace.columns()[0].width(), widths.0, "{name}");
            assert_eq!(
                workspace.effective_column_width(&workspace.columns()[0]),
                widths.1,
                "{name}"
            );
        }
    }

    #[test]
    fn failed_unchanged_landing_disables_physical_fast_path() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        let dispatched = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(100, 0, 400, 600),
            Visibility::Visible,
        )]);
        let (request_id, invalidation_id) = state.physical_request_ids();
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: dispatched[0].rect,
                requested_visibility: Visibility::Visible,
                actual_visible_rect: None,
                actual_outer_rect: None,
                failed: true,
                unreadable: false,
            }],
        );
        assert!(!state.last_physical_presentations[&100].confirmed);
        assert!(
            !state.physical_fast_path_ok(),
            "a failed unchanged placement must be retried rather than treated as native success"
        );
    }

    #[test]
    fn confirmed_unchanged_landing_allows_physical_fast_path() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        let dispatched = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(100, 0, 400, 600),
            Visibility::Visible,
        )]);
        let (request_id, invalidation_id) = state.physical_request_ids();
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: dispatched[0].rect,
                requested_visibility: Visibility::Visible,
                actual_visible_rect: Some(dispatched[0].rect),
                actual_outer_rect: Some(dispatched[0].rect),
                failed: false,
                unreadable: false,
            }],
        );
        assert!(state.pending_physical_presentations.is_empty());
        assert_eq!(state.expected_physical_rect(100), Some(dispatched[0].rect));
        assert!(state.last_physical_presentations[&100].confirmed);
        assert!(state.physical_fast_path_ok());
    }

    #[test]
    fn parked_landing_requires_actual_outer_clearance() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![
                monitor(1, 0, 0, 1920, 1080),
                monitor(2, 1920, 0, 1920, 1080),
            ],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        let logical = placement(100, Rect::new(1920, 0, 400, 600), Visibility::Visible);
        let dispatched = state.apply_physical_projection(vec![logical.clone()]);
        assert_eq!(
            state.pending_physical_presentations[&100].kind,
            PhysicalKind::Parked
        );
        let (request_id, invalidation_id) = state.physical_request_ids();
        let actual_outer = Rect::new(-20, -20, 2040, 1120);
        assert!(actual_outer.width > logical.rect.width);
        assert!(actual_outer.height > logical.rect.height);
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: dispatched[0].rect,
                requested_visibility: dispatched[0].visibility,
                actual_visible_rect: None,
                actual_outer_rect: Some(actual_outer),
                failed: false,
                unreadable: false,
            }],
        );

        assert!(!state.last_physical_presentations[&100].confirmed);
        assert!(state.is_physically_parked(100));
        assert!(!state.physical_fast_path_ok());
    }

    #[test]
    fn confirmed_landing_releases_a_revoked_ghost_source() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        let dispatched = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(100, 0, 400, 600),
            Visibility::Visible,
        )]);
        let (request_id, invalidation_id) = state.physical_request_ids();
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: dispatched[0].rect,
                requested_visibility: Visibility::Visible,
                actual_visible_rect: Some(dispatched[0].rect),
                actual_outer_rect: Some(dispatched[0].rect),
                failed: false,
                unreadable: false,
            }],
        );
        state.ghost_sources_pending_safe_landing.insert(100);

        state.release_ghost_sources_after_physical_landing();

        assert!(!state.ghost_sources_pending_safe_landing.contains(&100));
    }

    #[test]
    fn synthetic_exit_uses_snapshot_provenance_after_membership_changes() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![
                monitor(1, 0, 0, 1920, 1080),
                monitor(2, 1920, 0, 1920, 1080),
            ],
        );
        state.reduce_motion = false;
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        state.start_workspace_switch_transition(
            std::collections::HashMap::from([(100, Rect::new(1800, 0, 400, 600))]),
            std::collections::HashMap::from([(100, Rect::new(1800, -1080, 400, 600))]),
            150,
        );
        let _ = state.workspaces.get_mut(&1).unwrap()[0].remove_window(100);

        let dispatched = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(1800, 0, 400, 600),
            Visibility::Visible,
        )]);

        assert_eq!(dispatched[0].rect, Rect::new(1800, 0, 400, 600));
        assert_eq!(
            state.last_physical_presentations.get(&100).map(|_| ()),
            None,
            "projection remains pending until its landing is consumed"
        );
        assert_eq!(
            state.pending_physical_presentations[&100].kind,
            PhysicalKind::Unchanged,
            "a tiled exit remains eligible without shrinking its native geometry"
        );
    }

    #[test]
    fn synthetic_exit_preserves_original_floating_exemption() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![
                monitor(1, 0, 0, 1920, 1080),
                monitor(2, 1920, 0, 1920, 1080),
            ],
        );
        state.reduce_motion = false;
        state.workspaces.get_mut(&1).unwrap()[0]
            .add_floating(200, Rect::new(1800, 0, 400, 600))
            .unwrap();
        state.start_workspace_switch_transition(
            std::collections::HashMap::from([(200, Rect::new(1800, 0, 400, 600))]),
            std::collections::HashMap::from([(200, Rect::new(1800, -1080, 400, 600))]),
            150,
        );
        state.workspaces.get_mut(&1).unwrap()[0].remove_floating(200);

        let dispatched = state.apply_physical_projection(vec![placement(
            200,
            Rect::new(1800, 0, 400, 600),
            Visibility::Visible,
        )]);

        assert_eq!(dispatched[0].rect, Rect::new(1800, 0, 400, 600));
        assert_eq!(
            state.pending_physical_presentations[&200].kind,
            PhysicalKind::Unchanged,
            "a synthetic exit must retain its source floating exemption"
        );
    }

    #[test]
    fn synthetic_exit_preserves_original_native_and_other_exemptions() {
        use crate::state::{ApplicationFullscreenState, DragPreviewMode, DragState};

        let adjacent_monitors = || {
            vec![
                monitor(1, 0, 0, 1920, 1080),
                monitor(2, 1920, 0, 1920, 1080),
            ]
        };
        let exit_rect = Rect::new(1800, -1080, 400, 600);
        let boundary_rect = Rect::new(1800, 0, 400, 600);

        let mut application_fullscreen =
            AppState::new_with_config(crate::config::Config::default(), adjacent_monitors());
        application_fullscreen.reduce_motion = false;
        application_fullscreen.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(300, Some(800))
            .unwrap();
        application_fullscreen.application_fullscreen.insert(
            300,
            ApplicationFullscreenState {
                monitor_id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
            },
        );
        application_fullscreen.start_workspace_switch_transition(
            std::collections::HashMap::from([(300, boundary_rect)]),
            std::collections::HashMap::from([(300, exit_rect)]),
            150,
        );
        application_fullscreen.application_fullscreen.remove(&300);
        let _ = application_fullscreen.workspaces.get_mut(&1).unwrap()[0].remove_window(300);
        let dispatched = application_fullscreen.apply_physical_projection(vec![placement(
            300,
            boundary_rect,
            Visibility::Visible,
        )]);
        assert!(
            !application_fullscreen
                .layout_transition
                .as_ref()
                .unwrap()
                .exit_provenance[&300]
                .eligible
        );
        assert_eq!(
            application_fullscreen
                .layout_transition
                .as_ref()
                .unwrap()
                .exit_provenance[&300]
                .owner,
            1
        );
        assert_eq!(dispatched[0].rect, boundary_rect);
        assert_eq!(
            application_fullscreen.pending_physical_presentations[&300].kind,
            PhysicalKind::Unchanged
        );

        let mut layout_fullscreen =
            AppState::new_with_config(crate::config::Config::default(), adjacent_monitors());
        layout_fullscreen.reduce_motion = false;
        let workspace = &mut layout_fullscreen.workspaces.get_mut(&1).unwrap()[0];
        workspace.insert_window(400, Some(800)).unwrap();
        assert!(workspace.toggle_fullscreen());
        layout_fullscreen.start_workspace_switch_transition(
            std::collections::HashMap::from([(400, boundary_rect)]),
            std::collections::HashMap::from([(400, exit_rect)]),
            150,
        );
        let _ = layout_fullscreen.workspaces.get_mut(&1).unwrap()[0].remove_window(400);
        let dispatched = layout_fullscreen.apply_physical_projection(vec![placement(
            400,
            boundary_rect,
            Visibility::Visible,
        )]);
        assert!(
            !layout_fullscreen
                .layout_transition
                .as_ref()
                .unwrap()
                .exit_provenance[&400]
                .eligible
        );
        assert_eq!(
            layout_fullscreen
                .layout_transition
                .as_ref()
                .unwrap()
                .exit_provenance[&400]
                .owner,
            1
        );
        assert_eq!(dispatched[0].rect, boundary_rect);
        assert_eq!(
            layout_fullscreen.pending_physical_presentations[&400].kind,
            PhysicalKind::Unchanged
        );

        let mut dragging =
            AppState::new_with_config(crate::config::Config::default(), adjacent_monitors());
        dragging.reduce_motion = false;
        dragging.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(500, Some(800))
            .unwrap();
        dragging.drag_state = Some(DragState {
            hwnd: 500,
            is_tiled: true,
            source_monitor: 1,
            source_workspace_idx: 0,
            source_window_slot: 0,
            current_column_index: 0,
            last_drop_target: None,
            last_hint_update: None,
            removed_from_source: false,
            preview_mode: DragPreviewMode::None,
            target_column_peers: Vec::new(),
            source_column_peers: Vec::new(),
        });
        dragging.start_workspace_switch_transition(
            std::collections::HashMap::from([(500, boundary_rect)]),
            std::collections::HashMap::from([(500, exit_rect)]),
            150,
        );
        dragging.drag_state = None;
        let _ = dragging.workspaces.get_mut(&1).unwrap()[0].remove_window(500);
        let dispatched = dragging.apply_physical_projection(vec![placement(
            500,
            boundary_rect,
            Visibility::Visible,
        )]);
        assert!(!dragging.layout_transition.as_ref().unwrap().exit_provenance[&500].eligible);
        assert_eq!(
            dragging.layout_transition.as_ref().unwrap().exit_provenance[&500].owner,
            1
        );
        assert_eq!(dispatched[0].rect, boundary_rect);
        assert_eq!(
            dragging.pending_physical_presentations[&500].kind,
            PhysicalKind::Unchanged
        );

        let mut native_maximized =
            AppState::new_with_config(crate::config::Config::default(), adjacent_monitors());
        native_maximized.reduce_motion = false;
        native_maximized.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(600, Some(800))
            .unwrap();
        native_maximized.injected_window_maximized.insert(600, true);
        native_maximized.start_workspace_switch_transition(
            std::collections::HashMap::from([(600, boundary_rect)]),
            std::collections::HashMap::from([(600, exit_rect)]),
            150,
        );
        native_maximized.injected_window_maximized.remove(&600);
        let _ = native_maximized.workspaces.get_mut(&1).unwrap()[0].remove_window(600);
        let dispatched = native_maximized.apply_physical_projection(vec![placement(
            600,
            boundary_rect,
            Visibility::Visible,
        )]);
        let provenance = &native_maximized
            .layout_transition
            .as_ref()
            .unwrap()
            .exit_provenance[&600];
        assert_eq!(provenance.owner, 1);
        assert!(!provenance.eligible);
        assert_eq!(dispatched[0].rect, boundary_rect);
        assert_eq!(
            native_maximized.pending_physical_presentations[&600].kind,
            PhysicalKind::Unchanged
        );
    }

    #[test]
    fn pending_presentation_precedes_last_until_full_landing_is_consumed() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 5120, 1440), monitor(2, 5120, 0, 800, 600)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(1600))
            .unwrap();
        let logical = placement(100, reproduction_window(), Visibility::Visible);
        let dispatched = state.apply_physical_projection(vec![logical.clone()]);
        let mut previous = state.pending_physical_presentations[&100].clone();
        previous.physical.rect = Rect::new(0, 0, 400, 600);
        previous.confirmed = true;
        state.last_physical_presentations.insert(100, previous);
        assert_eq!(state.expected_physical_rect(100), Some(logical.rect));

        let (request_id, invalidation_id) = state.physical_request_ids();
        state.consume_physical_landings(
            request_id,
            invalidation_id,
            &[PlacementLanding {
                window_id: 100,
                requested_rect: logical.rect,
                requested_visibility: Visibility::Visible,
                actual_visible_rect: Some(logical.rect),
                actual_outer_rect: Some(logical.rect),
                failed: false,
                unreadable: false,
            }],
        );
        assert!(state.pending_physical_presentations.is_empty());
        assert_eq!(state.expected_physical_rect(100), Some(dispatched[0].rect));
        assert!(state.last_physical_presentations[&100].confirmed);
    }

    #[test]
    fn stale_landing_consumption_preserves_a_newer_pending_presentation() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        let _first = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(100, 0, 400, 600),
            Visibility::Visible,
        )]);
        let (old_request, old_invalidation) = state.physical_request_ids();
        let newer = state.apply_physical_projection(vec![placement(
            100,
            Rect::new(200, 0, 400, 600),
            Visibility::Visible,
        )]);
        state.consume_physical_landings(old_request, old_invalidation, &[]);
        assert_eq!(state.expected_physical_rect(100), Some(newer[0].rect));
        assert_eq!(
            state.pending_physical_request_id,
            old_request.wrapping_add(1)
        );
    }

    #[test]
    fn reproduction_right_overflow_is_sliced_to_owner_edge() {
        let window = reproduction_window();
        let owner = owner_5120();
        let neighbor = neighbor_800();
        let decision = project_physical_rect(window, owner, &[owner, neighbor]);
        match decision {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(4320, 10, 800, 1440));
                let overlap_w = edge_end(window.x, window.width) - neighbor.x as i64;
                let overlap_h = overlap_len(
                    window.y as i64,
                    edge_end(window.y, window.height),
                    neighbor.y as i64,
                    edge_end(neighbor.y, neighbor.height),
                );
                assert_eq!((overlap_w, overlap_h), (800, 590));
            }
            other => panic!("expected constrained slice, got {other:?}"),
        }
    }

    #[test]
    fn slices_left_top_and_bottom_neighbors() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let left = Rect::new(-800, 0, 800, 1000);
        let top = Rect::new(0, -600, 1000, 600);
        let bottom = Rect::new(0, 1000, 1000, 600);
        let overflow_left = Rect::new(-200, 100, 500, 400);
        match project_physical_rect(overflow_left, owner, &[owner, left]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(0, 100, 300, 400));
            }
            other => panic!("{other:?}"),
        }
        let overflow_top = Rect::new(100, -200, 400, 500);
        match project_physical_rect(overflow_top, owner, &[owner, top]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(100, 0, 400, 300));
            }
            other => panic!("{other:?}"),
        }
        let overflow_bottom = Rect::new(100, 800, 400, 400);
        match project_physical_rect(overflow_bottom, owner, &[owner, bottom]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(100, 800, 400, 200));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn partial_orthogonal_overlap_is_required() {
        let owner = owner_5120();
        let neighbor = neighbor_800();
        let no_y_overlap = Rect::new(4320, 700, 1600, 700);
        assert_eq!(
            project_physical_rect(no_y_overlap, owner, &[owner, neighbor]),
            PhysicalDecision::Unchanged
        );
    }

    #[test]
    fn two_axes_and_multiple_neighbors_clip_independently() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let right = Rect::new(1000, 0, 800, 1000);
        let bottom = Rect::new(0, 1000, 1000, 600);
        let window = Rect::new(800, 800, 400, 400);
        match project_physical_rect(window, owner, &[owner, right, bottom]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(800, 800, 200, 200));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn negative_origins_and_mixed_scale_physical_pixels() {
        let owner = Rect::new(-1920, 0, 1920, 1080);
        let neighbor = Rect::new(0, 0, 2880, 1800);
        let window = Rect::new(-200, 0, 500, 1080);
        match project_physical_rect(window, owner, &[owner, neighbor]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect, Rect::new(-200, 0, 200, 1080));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gaps_mirrors_and_corner_only_contacts_are_unchanged() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let gapped = Rect::new(1010, 0, 800, 1000);
        let mirrored = Rect::new(0, 0, 1000, 1000);
        let corner = Rect::new(1000, 1000, 800, 600);
        let window = Rect::new(900, 900, 200, 200);
        assert_eq!(
            project_physical_rect(window, owner, &[owner, gapped]),
            PhysicalDecision::Unchanged
        );
        assert_eq!(
            project_physical_rect(window, owner, &[owner, mirrored]),
            PhysicalDecision::Unchanged
        );
        assert_eq!(
            project_physical_rect(window, owner, &[owner, corner]),
            PhysicalDecision::Unchanged
        );
    }

    #[test]
    fn zero_and_one_pixel_slices_and_idempotence() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let neighbor = Rect::new(1000, 0, 800, 1000);
        let fully_past = Rect::new(1000, 0, 400, 400);
        match project_physical_rect(fully_past, owner, &[owner, neighbor]) {
            PhysicalDecision::Parked { rect } => {
                assert!(!rect.intersects(&owner));
                assert!(!rect.intersects(&neighbor));
            }
            other => panic!("{other:?}"),
        }
        let one_pixel = Rect::new(999, 0, 400, 400);
        match project_physical_rect(one_pixel, owner, &[owner, neighbor]) {
            PhysicalDecision::Constrained { rect, .. } => {
                assert_eq!(rect, Rect::new(999, 0, 1, 400));
                assert_eq!(
                    project_physical_rect(rect, owner, &[owner, neighbor]),
                    PhysicalDecision::Unchanged
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn zero_slice_parking_moves_past_a_monitor_near_the_recovery_sentinel() {
        let owner = Rect::new(-100_000, -100_000, 500, 500);
        let neighbor = Rect::new(-99_500, -100_000, 500, 500);
        let window = Rect::new(-99_500, -100_000, 400, 400);
        let insets = (7, 1, 7, 8);
        let parked = match decide_physical_rect(window, owner, &[owner, neighbor], insets) {
            PhysicalDecision::Parked { rect } => rect,
            other => panic!("expected zero visible slice to park, got {other:?}"),
        };
        let outer = leopardwm_platform_win32::visible_rect_to_frame_rect(parked, insets, false);
        assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
            &outer
        ));
        assert!(parking_clears_monitors(outer, &[owner, neighbor]));
    }

    #[test]
    fn arithmetic_extremes_do_not_overflow() {
        let owner = Rect::new(i32::MIN / 2, 0, 1000, 1000);
        let neighbor = Rect::new(i32::MIN / 2 + 1000, 0, 1000, 1000);
        let window = Rect::new(i32::MIN / 2 + 900, 0, 400, 400);
        let decision = project_physical_rect(window, owner, &[owner, neighbor]);
        assert!(matches!(
            decision,
            PhysicalDecision::Constrained { .. } | PhysicalDecision::Parked { .. }
        ));
    }

    #[test]
    fn parks_retained_native_size_larger_than_slice() {
        let owner = owner_5120();
        let neighbor = neighbor_800();
        let window = Rect::new(5000, 10, 800, 1440);
        let slice = match project_physical_rect(window, owner, &[owner, neighbor]) {
            PhysicalDecision::Constrained { rect, .. } => rect,
            other => panic!("{other:?}"),
        };
        assert!(slice.width < window.width);
        let parked = offscreen_park_rect(window, &[owner, neighbor], (7, 1, 7, 8));
        assert_eq!((parked.width, parked.height), (window.width, window.height));
        let outer =
            leopardwm_platform_win32::visible_rect_to_frame_rect(parked, (7, 1, 7, 8), false);
        assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
            &outer
        ));
        assert!(parking_clears_monitors(outer, &[owner, neighbor]));
    }

    #[test]
    fn empty_acknowledgement_ignores_stale_and_superseded_requests() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.apply_physical_projection(Vec::new());
        let (stale_request, stale_invalidation) = state.physical_request_ids();
        let applied = state.last_applied_physical_invalidation;
        let topology = state.last_topology_signature.clone();

        state.bump_physical_invalidation();
        state.acknowledge_empty_physical_state(stale_request, stale_invalidation, true);
        assert_eq!(state.last_applied_physical_invalidation, applied);
        assert_eq!(state.last_topology_signature, topology);
        assert_eq!(state.pending_physical_request_id, stale_request);
        assert_eq!(state.inflight_request_id, Some(stale_request));
        assert!(state.inflight_origins.contains_key(&stale_request));

        state.apply_physical_projection(Vec::new());
        let (newer_request, newer_invalidation) = state.physical_request_ids();
        assert_ne!(newer_request, stale_request);
        state.acknowledge_empty_physical_state(stale_request, stale_invalidation, true);
        assert_eq!(state.pending_physical_request_id, newer_request);
        assert_eq!(state.pending_physical_invalidation_id, newer_invalidation);
        assert_eq!(state.inflight_request_id, Some(newer_request));
        assert!(state.inflight_origins.contains_key(&newer_request));
        assert!(!state.inflight_origins.contains_key(&stale_request));
        assert_eq!(state.last_applied_physical_invalidation, applied);
        assert_eq!(state.last_topology_signature, topology);
    }

    #[test]
    fn empty_acknowledgement_is_fail_closed_without_logical_emptiness_or_prior_evidence() {
        let mut state = AppState::new_with_config(
            crate::config::Config::default(),
            vec![monitor(1, 0, 0, 1920, 1080)],
        );
        state.apply_physical_projection(Vec::new());
        let (empty_request, empty_invalidation) = state.physical_request_ids();
        let applied = state.last_applied_physical_invalidation;
        let topology = state.last_topology_signature.clone();
        state.acknowledge_empty_physical_state(empty_request, empty_invalidation, false);
        assert_eq!(state.last_applied_physical_invalidation, applied);
        assert_eq!(state.last_topology_signature, topology);
        assert_eq!(state.pending_physical_request_id, empty_request);

        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(100, Some(800))
            .unwrap();
        state.apply_physical_projection(vec![placement(
            100,
            Rect::new(0, 0, 800, 1040),
            Visibility::Visible,
        )]);
        let (failed_request, failed_invalidation) = state.physical_request_ids();
        state.abandon_physical_request(failed_request, failed_invalidation);
        assert!(!state.last_physical_presentations[&100].confirmed);
        let applied = state.last_applied_physical_invalidation;
        let topology = state.last_topology_signature.clone();
        state.apply_physical_projection(Vec::new());
        let (later_request, later_invalidation) = state.physical_request_ids();
        state.acknowledge_empty_physical_state(later_request, later_invalidation, true);
        assert_eq!(state.last_applied_physical_invalidation, applied);
        assert_eq!(state.last_topology_signature, topology);
        assert!(!state.last_physical_presentations[&100].confirmed);
        assert_eq!(state.pending_physical_request_id, later_request);
        assert!(state.inflight_origins.contains_key(&later_request));
    }

    #[test]
    fn tab_strip_uses_its_own_orthogonal_span() {
        let owner = Rect::new(0, 0, 1000, 1000);
        let above = Rect::new(0, -40, 1000, 40);
        let window = Rect::new(100, 0, 400, 800);
        assert_eq!(
            project_physical_rect(window, owner, &[owner, above]),
            PhysicalDecision::Unchanged
        );
        let strip = tab_strip_rect(window, 28, 8);
        match project_physical_rect(strip, owner, &[owner, above]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect.y, 0);
                assert_eq!(rect.height, 0.max(strip.height - 36 + 8));
            }
            PhysicalDecision::Parked { .. } => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parks_at_recoverable_sentinel_when_a_horizontal_neighbor_blocks_the_side() {
        const WIN: Rect = Rect {
            x: 6000,
            y: 10,
            width: 800,
            height: 600,
        };
        let owner = owner_5120();
        let right = Rect::new(5120, 0, 1920, 1080);
        let parked = offscreen_park_rect(WIN, &[owner, right], (0, 0, 0, 0));
        let sentinel = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
        assert_eq!((parked.x, parked.y), (sentinel, sentinel));
        assert_eq!((parked.width, parked.height), (WIN.width, WIN.height));
        assert!(![owner, right].iter().any(|m| parked.intersects(m)));
    }

    #[test]
    fn parks_at_recoverable_sentinel_when_stacked_vertically() {
        const WIN: Rect = Rect {
            x: 6000,
            y: 10,
            width: 800,
            height: 600,
        };
        let owner = Rect::new(0, 1080, 1920, 1080);
        let above = Rect::new(0, 0, 1920, 1080);
        let below = Rect::new(0, 2160, 1920, 1080);
        let parked = offscreen_park_rect(WIN, &[owner, above, below], (0, 0, 0, 0));
        let sentinel = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
        assert_eq!((parked.x, parked.y), (sentinel, sentinel));
        assert!(![owner, above, below].iter().any(|m| parked.intersects(m)));
    }

    #[test]
    fn parks_at_recoverable_sentinel_when_other_edges_are_taken() {
        const WIN: Rect = Rect {
            x: 6000,
            y: 10,
            width: 800,
            height: 600,
        };
        let owner = Rect::new(2000, 0, 1000, 1000);
        let below = Rect::new(2000, 1000, 1000, 1000);
        let above = Rect::new(2000, -1000, 1000, 1000);
        let right = Rect::new(3000, 0, 1000, 1000);
        let parked = offscreen_park_rect(WIN, &[owner, below, above, right], (0, 0, 0, 0));
        let sentinel = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
        assert_eq!((parked.x, parked.y), (sentinel, sentinel));
        assert!(![owner, below, above, right]
            .iter()
            .any(|m| parked.intersects(m)));
    }

    #[test]
    fn falls_back_to_the_far_sentinel_when_boxed_in_on_all_sides() {
        const WIN: Rect = Rect {
            x: 6000,
            y: 10,
            width: 800,
            height: 600,
        };
        let owner = Rect::new(0, 0, 1000, 1000);
        let neighbors = [
            owner,
            Rect::new(-2000, 0, 2000, 1000),
            Rect::new(1000, 0, 2000, 1000),
            Rect::new(0, -2000, 1000, 2000),
            Rect::new(0, 1000, 1000, 2000),
        ];
        let parked = offscreen_park_rect(WIN, &neighbors, (0, 0, 0, 0));
        let sentinel = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
        assert_eq!((parked.x, parked.y), (sentinel, sentinel));
    }

    #[test]
    fn wrapper_reparks_bleeding_hidden_and_leaves_projection_to_slice_visible() {
        let owner = monitor(1, 0, 0, 5120, 1440);
        let right = monitor(2, 5120, 0, 1920, 1080);
        let monitors: HashMap<MonitorId, MonitorInfo> = [(1, owner.clone()), (2, right.clone())]
            .into_iter()
            .collect();
        let visible = Rect::new(5200, 10, 400, 400);
        let non_bleed = Rect::new(-500, 10, 400, 400);
        let bleeding = Rect::new(5300, 10, 400, 400);
        let mut placements = vec![
            placement(10, visible, Visibility::Visible),
            placement(20, non_bleed, Visibility::OffScreenLeft),
            placement(30, bleeding, Visibility::OffScreenRight),
        ];
        park_offscreen_avoiding_neighbors(&mut placements, 1, &monitors);
        assert_eq!(
            placements[1].rect, non_bleed,
            "non-bleeding off-screen untouched"
        );
        assert_ne!(placements[2].rect, bleeding, "bleeding placement re-parked");
        assert!(![owner.rect, right.rect]
            .iter()
            .any(|m| placements[2].rect.intersects(m)));
        match project_physical_rect(visible, owner.rect, &[owner.rect, right.rect]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect.x + rect.width, owner.rect.x + owner.rect.width);
            }
            PhysicalDecision::Parked { .. } => {}
            other => panic!("visible crossing must be projected, got {other:?}"),
        }
    }

    #[test]
    fn wrapper_is_a_no_op_when_the_owner_monitor_is_missing() {
        let right = monitor(2, 5120, 0, 1920, 1080);
        let monitors: HashMap<MonitorId, MonitorInfo> = [(2, right)].into_iter().collect();
        let orig = Rect::new(5300, 10, 400, 400);
        let mut placements = vec![placement(30, orig, Visibility::OffScreenRight)];
        park_offscreen_avoiding_neighbors(&mut placements, 1, &monitors);
        assert_eq!(placements[0].rect, orig);
    }

    #[test]
    fn decorated_extent_reuses_projection() {
        let owner = owner_5120();
        let neighbor = neighbor_800();
        let content = Rect::new(5000, 10, 200, 400);
        let overlay = expand_border_overlay(content, 4, true);
        assert!(overlay.width > content.width);
        match project_physical_rect(overlay, owner, &[owner, neighbor]) {
            PhysicalDecision::Constrained { rect } => {
                assert_eq!(rect.x + rect.width, owner.x + owner.width);
            }
            other => panic!("{other:?}"),
        }
    }
}
