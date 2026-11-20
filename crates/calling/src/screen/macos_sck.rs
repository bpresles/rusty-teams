//! A ScreenCaptureKit stream that renders NV12 pixel buffers at the size the
//! encoder wants, on the GPU. The sample handler only parks the newest buffer;
//! the capture thread picks it up, so the dispatch queue never waits on WebRTC.

use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};

use super::native_plan::frame_interval;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`, the NV12 that VideoToolbox takes as is.
const PIXEL_FORMAT_NV12: u32 = 0x3432_3076;
/// ScreenCaptureKit accepts 3 to 8. The capture thread keeps the last frame
/// for idle resends, the encoder may still read one or two, one waits in the
/// slot: 6 leaves ScreenCaptureKit a free surface to render the next one into.
const QUEUE_DEPTH: isize = 6;
const STREAM_CHANGE_WAIT: Duration = Duration::from_secs(3);

/// A frame rendered by the GPU, as the capture thread sees it.
#[derive(Clone)]
pub struct PixelBuffer(pub CFRetained<CVPixelBuffer>);

// SAFETY: a `CVPixelBuffer` is reference counted and thread safe; nothing
// mutates it once ScreenCaptureKit delivered it.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

pub enum Wake {
    Frame(PixelBuffer),
    Quiet,
    Stopped,
}

#[derive(Default)]
struct SlotState {
    frame: Option<PixelBuffer>,
    stopped: bool,
}

/// The newest frame, waiting for the capture thread.
#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<SlotState>,
    wake: Condvar,
}

impl FrameSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn park(&self, frame: PixelBuffer) {
        self.lock().frame = Some(frame);
        self.wake.notify_one();
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.wake.notify_one();
    }

    pub fn wait(&self, timeout: Duration) -> Wake {
        let mut state = self.lock();
        if state.frame.is_none() && !state.stopped {
            state = self
                .wake
                .wait_timeout(state, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        match state.frame.take() {
            Some(frame) => Wake::Frame(frame),
            None if state.stopped => Wake::Stopped,
            None => Wake::Quiet,
        }
    }
}

struct SinkState {
    slot: Arc<FrameSlot>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = SinkState]
    #[name = "RustyTeamsShareSink"]
    struct ShareSink;

    unsafe impl NSObjectProtocol for ShareSink {}

    unsafe impl SCStreamOutput for ShareSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle, blank and suspended notifications carry no pixel buffer.
            // SAFETY: `sample` is valid for the duration of the callback.
            if let Some(image) = unsafe { sample.image_buffer() } {
                self.ivars().slot.park(PixelBuffer(image));
            }
        }
    }

    unsafe impl SCStreamDelegate for ShareSink {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            self.ivars().slot.stop();
        }
    }
);

impl ShareSink {
    fn new(slot: Arc<FrameSlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SinkState { slot });
        // SAFETY: `init` of `NSObject`, on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

fn configuration(size: (u32, u32), fps: u32) -> Retained<SCStreamConfiguration> {
    let (numerator, denominator) = frame_interval(fps);
    // SAFETY: plain setters on a configuration nobody else holds yet.
    unsafe {
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(size.0 as usize);
        configuration.setHeight(size.1 as usize);
        configuration.setPixelFormat(PIXEL_FORMAT_NV12);
        configuration.setMinimumFrameInterval(CMTime::new(numerator, denominator));
        configuration.setScalesToFit(true);
        configuration.setPreservesAspectRatio(true);
        configuration.setShowsCursor(true);
        configuration.setQueueDepth(QUEUE_DEPTH);
        configuration.setCapturesAudio(false);
        configuration
    }
}

/// A running capture. Dropping it stops the stream.
pub struct SckStream {
    stream: Retained<SCStream>,
    sink: Retained<ShareSink>,
    // Keeps the queue the handler runs on alive as long as the stream.
    _queue: DispatchRetained<DispatchQueue>,
    pub slot: Arc<FrameSlot>,
}

// SAFETY: the stream is only driven from the capture thread that owns it; the
// sink and the queue are used by ScreenCaptureKit itself, which synchronises them.
unsafe impl Send for SckStream {}

impl SckStream {
    pub fn start(filter: &SCContentFilter, size: (u32, u32), fps: u32) -> Result<Self, String> {
        let slot = Arc::new(FrameSlot::default());
        let sink = ShareSink::new(slot.clone());
        let queue = DispatchQueue::new("rusty-teams.share-frames", None);
        // SAFETY: the sink outlives the stream, which it is the delegate and output of.
        let stream = unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                &configuration(size, fps),
                Some(ProtocolObject::from_ref(&*sink)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*sink),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|error| error.localizedDescription().to_string())?;
            stream
        };
        let (sender, receiver) = channel();
        let started = RcBlock::new(move |error: *mut NSError| {
            let _ = sender.send(error_text(error));
        });
        // SAFETY: the block lives until the call returns and copies itself if it must outlive it.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        match receiver.recv_timeout(STREAM_CHANGE_WAIT) {
            Ok(None) => Ok(SckStream {
                stream,
                sink,
                _queue: queue,
                slot,
            }),
            Ok(Some(error)) => Err(error),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                Err("the capture did not start".to_owned())
            }
        }
    }

    /// Changes the rendered size and cadence without restarting the stream.
    pub fn retune(&self, size: (u32, u32), fps: u32) {
        let configuration = configuration(size, fps);
        // SAFETY: as for `start`; the completion is not awaited, a failure leaves the old setting.
        unsafe {
            self.stream
                .updateConfiguration_completionHandler(&configuration, None);
        }
    }
}

impl Drop for SckStream {
    fn drop(&mut self) {
        let (sender, receiver) = channel();
        let stopped = RcBlock::new(move |_error: *mut NSError| {
            let _ = sender.send(());
        });
        // SAFETY: stopping a stream we own; the output is removed whatever the answer.
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(&stopped));
            let _ = receiver.recv_timeout(STREAM_CHANGE_WAIT);
            let _ = self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.sink),
                SCStreamOutputType::Screen,
            );
        }
    }
}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: ScreenCaptureKit passes null or a valid error for the call's duration.
    unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string())
}
