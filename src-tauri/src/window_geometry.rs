//! Pure startup geometry policy. Inputs and outputs use physical pixels;
//! preferred sizes and minimum sizes are specified in logical pixels.

#[derive(Debug, PartialEq, Eq)]
pub(super) struct WindowGeometry {
    pub width: u32,
    pub height: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub x: i32,
    pub y: i32,
}

pub(super) fn initial_geometry(
    origin: [i32; 2],
    work_size: [u32; 2],
    scale_factor: f64,
    frame_size: [u32; 2],
) -> Option<WindowGeometry> {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return None;
    }
    let available_width = work_size[0].checked_sub(frame_size[0])?;
    let available_height = work_size[1].checked_sub(frame_size[1])?;
    if available_width == 0 || available_height == 0 {
        return None;
    }

    let logical_width = (f64::from(work_size[0]) / scale_factor * 0.28).clamp(400.0, 640.0);
    // Round down to avoid crossing a physical work-area edge at fractional DPI.
    let physical = |logical: f64, available: u32| {
        ((logical * scale_factor).floor() as u32)
            .max(1)
            .min(available)
    };
    let width = physical(logical_width, available_width);
    let height = physical(240.0, available_height);
    let centered = |axis: usize, inner: u32| {
        let remaining = work_size[axis] - frame_size[axis] - inner;
        (i64::from(origin[axis]) + i64::from(remaining / 2))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };

    Some(WindowGeometry {
        width,
        height,
        min_width: physical(360.0, available_width),
        min_height: physical(220.0, available_height),
        x: centered(0, width),
        y: centered(1, height),
    })
}

pub(super) fn rescale_size(size: [u32; 2], previous_scale: f64, current_scale: f64) -> [u32; 2] {
    if !previous_scale.is_finite()
        || previous_scale <= 0.0
        || !current_scale.is_finite()
        || current_scale <= 0.0
    {
        return size;
    }
    size.map(|value| ((f64::from(value) * current_scale / previous_scale).round() as u32).max(1))
}

/// Restore normal bounds, shrinking them to the current display work area.
/// A disconnected display or changed taskbar/DPI must not strand the title bar.
pub(super) fn restore_geometry(
    position: [i32; 2],
    size: [u32; 2],
    origin: [i32; 2],
    work_size: [u32; 2],
    scale_factor: f64,
    frame_size: [u32; 2],
) -> Option<WindowGeometry> {
    let mut geometry = initial_geometry(origin, work_size, scale_factor, frame_size)?;
    geometry.width = size[0].clamp(geometry.min_width, work_size[0] - frame_size[0]);
    geometry.height = size[1].clamp(geometry.min_height, work_size[1] - frame_size[1]);
    let [x, y] = constrain_position(
        position,
        [
            geometry.width + frame_size[0],
            geometry.height + frame_size[1],
        ],
        origin,
        work_size,
    );
    geometry.x = x;
    geometry.y = y;
    Some(geometry)
}

/// Preserve a user-selected position unless the outer window crosses an edge.
/// If a window is larger than the work area, keep its top-left controls visible.
pub(super) fn constrain_position(
    position: [i32; 2],
    outer_size: [u32; 2],
    origin: [i32; 2],
    work_size: [u32; 2],
) -> [i32; 2] {
    std::array::from_fn(|axis| {
        let start = i64::from(origin[axis]);
        let end = start + i64::from(work_size[axis].saturating_sub(outer_size[axis]));
        i64::from(position[axis])
            .clamp(start, end)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_size_preserves_logical_dimensions_across_dpi() {
        assert_eq!(rescale_size([800, 600], 1.25, 2.0), [1280, 960]);
        assert_eq!(rescale_size([800, 600], 0.0, 2.0), [800, 600]);
    }

    #[test]
    fn normal_display_uses_twenty_eight_percent_and_initial_height() {
        let geometry = initial_geometry([0, 0], [1920, 1040], 1.0, [16, 39]).unwrap();
        assert_eq!(
            geometry,
            WindowGeometry {
                width: 537,
                height: 240,
                min_width: 360,
                min_height: 220,
                x: 683,
                y: 380,
            }
        );
    }

    #[test]
    fn preferred_width_has_logical_lower_and_upper_bounds() {
        assert_eq!(
            initial_geometry([0, 0], [1280, 720], 1.0, [0, 0])
                .unwrap()
                .width,
            400
        );
        assert_eq!(
            initial_geometry([0, 0], [3440, 1440], 1.0, [0, 0])
                .unwrap()
                .width,
            640
        );
    }

    #[test]
    fn high_dpi_preserves_logical_size_not_physical_screen_percentage() {
        let geometry = initial_geometry([0, 0], [3840, 2080], 2.0, [32, 78]).unwrap();
        assert_eq!((geometry.width, geometry.height), (1075, 480));
        assert_eq!((geometry.min_width, geometry.min_height), (720, 440));
        // A 4K display at 200% is logically 1920 wide, not 3840 wide.
        assert!(geometry.width < 1280);
    }

    #[test]
    fn fractional_dpi_rounds_down_and_centers_on_offset_monitor() {
        let geometry = initial_geometry([-1920, 40], [1920, 1000], 1.25, [20, 49]).unwrap();
        assert_eq!((geometry.width, geometry.height), (537, 300));
        assert_eq!((geometry.x, geometry.y), (-1239, 365));
        assert_eq!((geometry.min_width, geometry.min_height), (450, 275));
    }

    #[test]
    fn tiny_work_area_relaxes_minimum_and_fits_outer_window() {
        let geometry = initial_geometry([15, 30], [320, 200], 1.5, [24, 58]).unwrap();
        assert_eq!((geometry.width, geometry.height), (296, 142));
        assert_eq!((geometry.min_width, geometry.min_height), (296, 142));
        assert_eq!((geometry.x, geometry.y), (15, 30));
    }

    #[test]
    fn dimensions_fit_work_area_across_scales_and_sizes() {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            for size in [[321, 201], [1280, 720], [1920, 1040], [3840, 2080]] {
                let geometry = initial_geometry([-100, 40], size, scale, [16, 39]).unwrap();
                assert!(geometry.min_width <= geometry.width);
                assert!(geometry.min_height <= geometry.height);
                assert!(geometry.width + 16 <= size[0]);
                assert!(geometry.height + 39 <= size[1]);
                assert!(geometry.x >= -100);
                assert!(geometry.y >= 40);
                assert!(
                    i64::from(geometry.x) + i64::from(geometry.width + 16)
                        <= -100 + i64::from(size[0])
                );
                assert!(
                    i64::from(geometry.y) + i64::from(geometry.height + 39)
                        <= 40 + i64::from(size[1])
                );
            }
        }
    }

    #[test]
    fn content_growth_moves_only_overflowing_axis() {
        assert_eq!(
            constrain_position([683, 380], [553, 839], [0, 0], [1920, 1040]),
            [683, 201]
        );
        assert_eq!(
            constrain_position([100, 150], [553, 300], [0, 0], [1920, 1040]),
            [100, 150]
        );
    }

    #[test]
    fn position_constraints_support_negative_origins_and_oversized_windows() {
        assert_eq!(
            constrain_position([-300, 100], [600, 500], [-1920, 40], [1920, 1000]),
            [-600, 100]
        );
        assert_eq!(
            constrain_position([-2000, 0], [600, 500], [-1920, 40], [1920, 1000]),
            [-1920, 40]
        );
        assert_eq!(
            constrain_position([100, 100], [600, 500], [10, 20], [320, 200]),
            [10, 20]
        );
    }

    #[test]
    fn restore_preserves_user_bounds_on_unchanged_display() {
        let geometry =
            restore_geometry([120, 80], [680, 500], [0, 0], [1920, 1040], 1.0, [16, 39]).unwrap();
        assert_eq!(
            (geometry.x, geometry.y, geometry.width, geometry.height),
            (120, 80, 680, 500)
        );
    }

    #[test]
    fn restore_from_disconnected_monitor_shrinks_and_repositions() {
        let geometry = restore_geometry(
            [-1800, 100],
            [1600, 1000],
            [0, 40],
            [1280, 680],
            1.5,
            [24, 58],
        )
        .unwrap();
        assert_eq!(
            (geometry.x, geometry.y, geometry.width, geometry.height),
            (0, 40, 1256, 622)
        );
    }

    #[test]
    fn restore_on_tiny_work_area_relaxes_minimum() {
        let geometry = restore_geometry(
            [900, 900],
            [400, 400],
            [-100, 20],
            [320, 200],
            2.0,
            [32, 78],
        )
        .unwrap();
        assert_eq!(
            (geometry.x, geometry.y, geometry.width, geometry.height),
            (-100, 20, 288, 122)
        );
    }

    #[test]
    fn unusable_monitor_data_keeps_existing_defaults() {
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(initial_geometry([0, 0], [1920, 1040], scale, [16, 39]).is_none());
        }
        assert!(initial_geometry([0, 0], [0, 0], 1.0, [0, 0]).is_none());
        assert!(initial_geometry([0, 0], [16, 39], 1.0, [16, 39]).is_none());
        assert!(initial_geometry([0, 0], [10, 20], 1.0, [16, 39]).is_none());
    }
}
