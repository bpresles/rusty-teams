//! A captured frame on its way to the encoder: pixels in memory, or on macOS a
//! GPU pixel buffer that never needs to touch the CPU.

use libwebrtc::video_frame::{I420Buffer, VideoBuffer as _};

use crate::share_load::FramePath;

#[cfg(target_os = "macos")]
use libwebrtc::video_frame::native::{NativeBuffer, VideoFrameBufferExt as _};

pub enum ShareFrame {
    I420(I420Buffer),
    #[cfg(target_os = "macos")]
    Native(NativeBuffer),
}

impl From<I420Buffer> for ShareFrame {
    fn from(buffer: I420Buffer) -> Self {
        ShareFrame::I420(buffer)
    }
}

#[cfg(target_os = "macos")]
impl From<NativeBuffer> for ShareFrame {
    fn from(buffer: NativeBuffer) -> Self {
        ShareFrame::Native(buffer)
    }
}

impl ShareFrame {
    pub fn size(&self) -> (u32, u32) {
        match self {
            ShareFrame::I420(buffer) => (buffer.width(), buffer.height()),
            #[cfg(target_os = "macos")]
            ShareFrame::Native(buffer) => (buffer.width(), buffer.height()),
        }
    }

    pub fn path(&self) -> FramePath {
        match self {
            ShareFrame::I420(_) => FramePath::CpuI420,
            #[cfg(target_os = "macos")]
            ShareFrame::Native(_) => FramePath::NativeNv12,
        }
    }

    /// The only place a native frame is copied to memory: effects that draw on
    /// the pixels, and the local preview, ask for it.
    pub fn into_i420(self) -> I420Buffer {
        match self {
            ShareFrame::I420(buffer) => buffer,
            #[cfg(target_os = "macos")]
            ShareFrame::Native(buffer) => buffer.to_i420(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_i420_frame_keeps_its_size_and_takes_the_cpu_path() {
        let frame = ShareFrame::from(I420Buffer::new(64, 32));
        assert_eq!(frame.size(), (64, 32));
        assert_eq!(frame.path(), FramePath::CpuI420);
        let buffer = frame.into_i420();
        assert_eq!((buffer.width(), buffer.height()), (64, 32));
    }
}
