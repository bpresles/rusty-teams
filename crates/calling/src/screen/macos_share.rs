//! The macOS share: system picker, then a ScreenCaptureKit stream whose NV12
//! frames go to the encoder untouched. `available` says whether this path can
//! run; if not the caller keeps the `DesktopCapturer` path.

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use libwebrtc::video_frame::native::NativeBuffer;
use objc2::runtime::AnyClass;
use objc2_core_foundation::CFRetained;
use objc2_foundation::{NSOperatingSystemVersion, NSProcessInfo};
use objc2_screen_capture_kit::SCContentFilter;

use super::macos_picker::{OpenPicker, PickedContent, PickerEvent};
use super::macos_sck::{PixelBuffer, SckStream, Wake};
use super::native_plan::{
    PickOutcome, follow_up, frame_interval, kind_for_style, resend_due, source_pixels, target_size,
};
use super::{PORTAL_UNAVAILABLE_NOTICE, PortalFollowUp, ShareFrame, ShareKind, ShareTuning};
use crate::capture::Capture;
use crate::error::{Error, Result};

/// `SCContentSharingPicker` and the pixel buffers it feeds need macOS 14.
const FIRST_MACOS: NSOperatingSystemVersion = NSOperatingSystemVersion {
    majorVersion: 14,
    minorVersion: 0,
    patchVersion: 0,
};
const LEGACY_SWITCH: &str = "CALLING_SCREEN_CAPTURE";
const POLL: Duration = Duration::from_millis(100);

/// Whether the native path can run here. `CALLING_SCREEN_CAPTURE=legacy` forces the old one.
pub fn available() -> bool {
    if std::env::var(LEGACY_SWITCH).is_ok_and(|value| value == "legacy") {
        return false;
    }
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(FIRST_MACOS)
        && AnyClass::get(c"SCContentSharingPicker").is_some()
}

fn wait_for_pick(
    kind: ShareKind,
    stop: &std::sync::atomic::AtomicBool,
) -> Option<(OpenPicker, PickerEvent)> {
    let (sender, receiver) = channel();
    let picker = OpenPicker::present(kind, sender);
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match receiver.recv_timeout(POLL) {
            Ok(event) => return Some((picker, event)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Some((picker, PickerEvent::Failed)),
        }
    }
}

fn kind_of(filter: &SCContentFilter) -> Option<ShareKind> {
    // SAFETY: a plain getter on a filter the picker handed over.
    kind_for_style(unsafe { filter.style() }.0 as i64)
}

fn source_size(filter: &SCContentFilter) -> (u32, u32) {
    // SAFETY: plain getters on a filter the picker handed over.
    let (rect, scale) = unsafe { (filter.contentRect(), filter.pointPixelScale()) };
    source_pixels(rect.size.width, rect.size.height, scale)
}

/// The encoder takes one reference over: the C++ side releases it once wrapped.
fn native_frame(buffer: &PixelBuffer) -> ShareFrame {
    let raw = CFRetained::into_raw(buffer.0.clone())
        .as_ptr()
        .cast::<c_void>();
    // SAFETY: `raw` is a retained `CVPixelBuffer`, and its reference moves to the buffer.
    ShareFrame::Native(unsafe { NativeBuffer::from_cv_pixel_buffer(raw) })
}

pub fn start(
    kind: ShareKind,
    tuning: ShareTuning,
    mut deliver: impl FnMut(ShareFrame) + Send + 'static,
    outcome: impl FnOnce(PortalFollowUp) + Send + 'static,
) -> Result<Capture> {
    Capture::spawn_thread("portal-share", move |stop| {
        let Some((picker, event)) = wait_for_pick(kind, &stop) else {
            return;
        };
        let (pick, filter) = match event {
            PickerEvent::Picked(PickedContent(filter)) => {
                (PickOutcome::Picked(kind_of(&filter)), Some(filter))
            }
            PickerEvent::Cancelled => (PickOutcome::Cancelled, None),
            PickerEvent::Failed => (PickOutcome::Failed, None),
        };
        drop(picker);
        let (Some(filter), PickOutcome::Picked(Some(picked_kind))) = (filter, pick) else {
            return outcome(follow_up(pick));
        };
        let source = source_size(&filter);
        let sized = |max_fs: u32| target_size(source, max_fs);
        let mut applied = (
            tuning.fps.load(Ordering::Relaxed),
            tuning.max_fs.load(Ordering::Relaxed),
        );
        let stream = match SckStream::start(&filter, sized(applied.1), applied.0) {
            Ok(stream) => stream,
            Err(_) => return outcome(PortalFollowUp::Notice(PORTAL_UNAVAILABLE_NOTICE)),
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        outcome(follow_up(PickOutcome::Picked(Some(picked_kind))));
        let mut last: Option<PixelBuffer> = None;
        let mut last_sent = Instant::now();
        while !stop.load(Ordering::SeqCst) {
            match stream.slot.wait(POLL) {
                Wake::Frame(frame) => {
                    deliver(native_frame(&frame));
                    last = Some(frame);
                    last_sent = Instant::now();
                }
                Wake::Quiet => {
                    if let Some(frame) = &last
                        && resend_due(last_sent.elapsed())
                    {
                        deliver(native_frame(frame));
                        last_sent = Instant::now();
                    }
                }
                Wake::Stopped => return,
            }
            let wanted = (
                tuning.fps.load(Ordering::Relaxed),
                tuning.max_fs.load(Ordering::Relaxed),
            );
            if wanted != applied {
                let changed_size = sized(wanted.1) != sized(applied.1);
                let changed_rate = frame_interval(wanted.0) != frame_interval(applied.0);
                if changed_size || changed_rate {
                    stream.retune(sized(wanted.1), wanted.0);
                }
                applied = wanted;
            }
        }
    })
    .map_err(|error| Error::Webrtc(format!("portal thread: {error}")))
}
