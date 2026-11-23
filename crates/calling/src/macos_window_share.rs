//! Native macOS ScreenCaptureKit single-window capture via `SCContentSharingPicker`.
//!
//! WebRTC's `DesktopCapturer::CreateWindowCapturer` always creates legacy
//! `WindowCapturerMac` (CGWindowList without a system picker), which fails
//! immediately in portal mode because no `CGWindowID` is preselected. This
//! module drives `SCContentSharingPicker` in `SingleWindow` mode directly in
//! `calling` without patching vendored `webrtc-sys`.

use std::ffi::c_void;
use std::ptr::NonNull;

type FrameFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    bgra: *const u8,
    available_bytes: usize,
    width: u32,
    height: u32,
    stride: u32,
);

unsafe extern "C" {
    fn rusty_teams_mac_sck_window_available() -> bool;
    fn rusty_teams_mac_window_capture_start(max_fps: u32, include_cursor: bool) -> *mut c_void;
    fn rusty_teams_mac_window_capture_set_fps(handle: *mut c_void, max_fps: u32);
    fn rusty_teams_mac_window_capture_poll(
        handle: *mut c_void,
        ctx: *mut c_void,
        callback: FrameFn,
    ) -> i32;
    fn rusty_teams_mac_window_capture_stop(handle: *mut c_void);
}

pub(crate) fn is_available() -> bool {
    unsafe { rusty_teams_mac_sck_window_available() }
}

pub(crate) enum WindowPoll<T> {
    Frame(T),
    Waiting,
    Cancelled,
    Failed,
}

pub(crate) struct MacWindowCapture {
    handle: NonNull<c_void>,
}

// The Objective-C session synchronizes all internal state and is polled from
// the single capture thread before Drop releases it.
unsafe impl Send for MacWindowCapture {}

impl MacWindowCapture {
    pub(crate) fn start(max_fps: u32, include_cursor: bool) -> Option<Self> {
        let raw = unsafe { rusty_teams_mac_window_capture_start(max_fps, include_cursor) };
        NonNull::new(raw).map(|handle| Self { handle })
    }

    pub(crate) fn set_fps(&self, max_fps: u32) {
        unsafe { rusty_teams_mac_window_capture_set_fps(self.handle.as_ptr(), max_fps) };
    }

    pub(crate) fn poll<T, F>(&self, mut convert: F) -> WindowPoll<T>
    where
        F: FnMut(&[u8], u32, u32, u32) -> Option<T>,
    {
        struct State<'a, T, F> {
            convert: &'a mut F,
            output: Option<T>,
        }

        unsafe extern "C" fn trampoline<T, F>(
            ctx: *mut c_void,
            bgra: *const u8,
            available_bytes: usize,
            width: u32,
            height: u32,
            stride: u32,
        ) where
            F: FnMut(&[u8], u32, u32, u32) -> Option<T>,
        {
            if bgra.is_null() || width < 2 || height < 2 {
                return;
            }
            let row_bytes = width as usize * 4;
            if (stride as usize) < row_bytes {
                return;
            }
            let min_needed = (height as usize - 1)
                .saturating_mul(stride as usize)
                .saturating_add(row_bytes);
            if available_bytes < min_needed {
                return;
            }
            let state = unsafe { &mut *(ctx as *mut State<'_, T, F>) };
            let full_needed = (height as usize).saturating_mul(stride as usize);
            if available_bytes >= full_needed {
                let bytes = unsafe { std::slice::from_raw_parts(bgra, full_needed) };
                state.output = (state.convert)(bytes, width, height, stride);
            } else {
                // Cropped sub-rect touching the bottom edge with x > 0: copy
                // tightly so downstream consumers can assume `stride * height`.
                let bytes = unsafe { std::slice::from_raw_parts(bgra, min_needed) };
                let mut tight = Vec::with_capacity(row_bytes * height as usize);
                for row in 0..height as usize {
                    let start = row * stride as usize;
                    tight.extend_from_slice(&bytes[start..start + row_bytes]);
                }
                state.output = (state.convert)(&tight, width, height, row_bytes as u32);
            }
        }

        let mut state = State {
            convert: &mut convert,
            output: None,
        };
        let code = unsafe {
            rusty_teams_mac_window_capture_poll(
                self.handle.as_ptr(),
                (&mut state as *mut State<'_, T, F>).cast::<c_void>(),
                trampoline::<T, F>,
            )
        };
        match code {
            1 => match state.output {
                Some(frame) => WindowPoll::Frame(frame),
                None => WindowPoll::Waiting,
            },
            0 => WindowPoll::Waiting,
            -1 => WindowPoll::Cancelled,
            _ => WindowPoll::Failed,
        }
    }
}

impl Drop for MacWindowCapture {
    fn drop(&mut self) {
        unsafe { rusty_teams_mac_window_capture_stop(self.handle.as_ptr()) };
    }
}
