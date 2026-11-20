//! The decisions of the native macOS share that do not need the OS: how big
//! the captured frames should be, and what a picker result means for the call.
//! Kept apart from the ScreenCaptureKit code so that they are tested everywhere.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::time::Duration;

use super::{PORTAL_UNAVAILABLE_NOTICE, PortalFollowUp, ShareKind, ShareSource};
use crate::send_control::scale_for;

/// What Teams allows for a share until it sends its own limit: about 1080p.
pub const DEFAULT_MAX_FS: u32 = 8160;

/// A static screen produces no frame. The encoder gets the last one again at
/// this pace, so that a viewer who joins late, or a lost frame, still resolves.
pub const IDLE_RESEND: Duration = Duration::from_secs(1);

/// `SCShareableContentStyle` of the picked content.
const STYLE_WINDOW: i64 = 1;
const STYLE_DISPLAY: i64 = 2;

/// Size in pixels of the content the user picked, from its size in points.
pub fn source_pixels(width_points: f64, height_points: f64, pixels_per_point: f32) -> (u32, u32) {
    let scale = f64::from(pixels_per_point.max(1.0));
    let pixels = |points: f64| (points.max(0.0) * scale).round() as u32;
    (pixels(width_points), pixels(height_points))
}

/// The size ScreenCaptureKit should render at: the source scaled down so that
/// it fits `max_fs` macroblocks, with even sides. Sent as is, WebRTC then has
/// nothing left to scale, which is what keeps the CPU out of the loop.
pub fn target_size(source: (u32, u32), max_fs: u32) -> (u32, u32) {
    let (width, height) = source;
    let max_fs = if max_fs == 0 { DEFAULT_MAX_FS } else { max_fs };
    let scale = scale_for(width, height, max_fs);
    let even = |side: u32| ((f64::from(side) / scale) as u32 & !1).max(2);
    (even(width), even(height))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickOutcome {
    Picked(Option<ShareKind>),
    Cancelled,
    Failed,
}

pub fn kind_for_style(style: i64) -> Option<ShareKind> {
    match style {
        STYLE_WINDOW => Some(ShareKind::Window),
        STYLE_DISPLAY => Some(ShareKind::Screen),
        _ => None,
    }
}

/// The share the call proceeds with. The native capture already owns the
/// stream, so the source carries no id to look up again.
pub fn follow_up(outcome: PickOutcome) -> PortalFollowUp {
    match outcome {
        PickOutcome::Picked(Some(kind)) => PortalFollowUp::Share(ShareSource {
            id: 0,
            kind,
            title: String::new(),
        }),
        PickOutcome::Picked(None) | PickOutcome::Failed => {
            PortalFollowUp::Notice(PORTAL_UNAVAILABLE_NOTICE)
        }
        PickOutcome::Cancelled => PortalFollowUp::Quiet,
    }
}

/// `minimumFrameInterval` as a rational: one frame every `1/fps` seconds.
pub fn frame_interval(fps: u32) -> (i64, i32) {
    (1, i32::try_from(fps.clamp(1, 120)).unwrap_or(1))
}

pub fn resend_due(since_last_frame: Duration) -> bool {
    since_last_frame >= IDLE_RESEND
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send_control::capture_macroblocks;

    fn blocks(size: (u32, u32)) -> u32 {
        capture_macroblocks(size.0, size.1)
    }

    #[test]
    fn a_5k_screen_is_rendered_at_the_size_teams_asks_for() {
        let size = target_size((5120, 2880), 8160);
        assert!(blocks(size) <= 8160, "{size:?}");
        assert!(blocks(size) > 7000, "no point in going far below: {size:?}");
        assert_eq!(size.0 % 2 + size.1 % 2, 0);
        let ratio = f64::from(size.0) / f64::from(size.1);
        assert!((ratio - 16. / 9.).abs() < 0.02, "{ratio}");
    }

    #[test]
    fn a_source_that_already_fits_is_not_touched_or_enlarged() {
        assert_eq!(target_size((1280, 720), 8160), (1280, 720));
        assert_eq!(target_size((1279, 719), 8160), (1278, 718));
    }

    #[test]
    fn the_limit_of_the_server_wins_over_the_default() {
        let size = target_size((1920, 1080), 3600);
        assert!(blocks(size) <= 3600, "{size:?}");
        assert!(size.0 < 1920);
    }

    #[test]
    fn an_unknown_limit_falls_back_to_the_teams_default() {
        assert_eq!(
            target_size((3880, 2520), 0),
            target_size((3880, 2520), DEFAULT_MAX_FS)
        );
    }

    #[test]
    fn a_tall_or_tiny_source_keeps_even_sides_of_at_least_two() {
        let size = target_size((1000, 3000), 8160);
        assert_eq!((size.0 % 2, size.1 % 2), (0, 0));
        assert!(blocks(size) <= 8160);
        assert_eq!(target_size((1, 1), 8160), (2, 2));
    }

    #[test]
    fn points_become_pixels_with_the_screen_scale() {
        assert_eq!(source_pixels(2560., 1440., 2.0), (5120, 2880));
        assert_eq!(source_pixels(1440., 900., 0.0), (1440, 900));
    }

    #[test]
    fn the_picked_content_style_gives_the_share_kind() {
        assert_eq!(kind_for_style(1), Some(ShareKind::Window));
        assert_eq!(kind_for_style(2), Some(ShareKind::Screen));
        assert_eq!(kind_for_style(0), None);
        assert_eq!(kind_for_style(3), None);
    }

    #[test]
    fn a_pick_shares_a_cancel_stays_quiet_and_a_failure_is_a_notice() {
        assert!(matches!(
            follow_up(PickOutcome::Picked(Some(ShareKind::Window))),
            PortalFollowUp::Share(source) if source.kind == ShareKind::Window
        ));
        assert_eq!(follow_up(PickOutcome::Cancelled), PortalFollowUp::Quiet);
        assert_eq!(
            follow_up(PickOutcome::Failed),
            PortalFollowUp::Notice(PORTAL_UNAVAILABLE_NOTICE)
        );
        assert_eq!(
            follow_up(PickOutcome::Picked(None)),
            PortalFollowUp::Notice(PORTAL_UNAVAILABLE_NOTICE)
        );
    }

    #[test]
    fn the_frame_interval_follows_the_share_rate() {
        assert_eq!(frame_interval(15), (1, 15));
        assert_eq!(frame_interval(30), (1, 30));
        assert_eq!(frame_interval(0), (1, 1));
    }

    #[test]
    fn a_still_screen_resends_its_last_frame_once_a_second() {
        assert!(!resend_due(Duration::from_millis(999)));
        assert!(resend_due(IDLE_RESEND));
    }
}
