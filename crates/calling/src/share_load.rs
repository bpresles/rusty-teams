//! What a screen share costs, frame by frame, summed up for the support bundle.
//!
//! The capture thread, the sink and the call loop each time their own step and
//! drop the result here. At most one compact line leaves every `REPORT_EVERY`,
//! with averages and worst cases only: no pixels, titles or account data.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REPORT_EVERY: Duration = Duration::from_secs(10);

/// How the frames of a share reach the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePath {
    /// Captured as BGRA, converted to I420 on the CPU, scaled by WebRTC.
    CpuI420,
    /// ScreenCaptureKit hands over a GPU-scaled NV12 pixel buffer, untouched.
    NativeNv12,
}

impl FramePath {
    fn label(self) -> &'static str {
        match self {
            FramePath::CpuI420 => "cpu-i420",
            FramePath::NativeNv12 => "native-nv12",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RunningStat {
    count: u64,
    total_us: u64,
    max_us: u64,
}

impl RunningStat {
    fn record(&mut self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.count += 1;
        self.total_us = self.total_us.saturating_add(micros);
        self.max_us = self.max_us.max(micros);
    }

    fn describe(&self) -> String {
        if self.count == 0 {
            return "-".to_owned();
        }
        let average = self.total_us as f64 / self.count as f64 / 1000.;
        format!("{average:.1}/{:.1}ms", self.max_us as f64 / 1000.)
    }
}

#[derive(Debug)]
struct Window {
    started: Instant,
    frames: u64,
    size: Option<(u32, u32)>,
    path: Option<FramePath>,
    grab: RunningStat,
    convert: RunningStat,
    push: RunningStat,
    handoff: RunningStat,
    preview: RunningStat,
    gap: RunningStat,
    stats_poll: RunningStat,
}

impl Window {
    fn starting(now: Instant) -> Self {
        Window {
            started: now,
            frames: 0,
            size: None,
            path: None,
            grab: RunningStat::default(),
            convert: RunningStat::default(),
            push: RunningStat::default(),
            handoff: RunningStat::default(),
            preview: RunningStat::default(),
            gap: RunningStat::default(),
            stats_poll: RunningStat::default(),
        }
    }

    fn is_empty(&self) -> bool {
        self.frames == 0 && self.stats_poll.count == 0
    }

    fn describe(&self, now: Instant) -> String {
        let seconds = now.duration_since(self.started).as_secs_f64().max(0.001);
        let size = self.size.map_or_else(
            || "-".to_owned(),
            |(width, height)| format!("{width}x{height}"),
        );
        format!(
            "path={} size={size} fps={:.1} gap={} grab={} convert={} push={} handoff={} preview={} stats-poll={}",
            self.path.map_or("-", FramePath::label),
            self.frames as f64 / seconds,
            self.gap.describe(),
            self.grab.describe(),
            self.convert.describe(),
            self.push.describe(),
            self.handoff.describe(),
            self.preview.describe(),
            self.stats_poll.describe(),
        )
    }
}

struct State {
    window: Window,
    last_frame: Option<Instant>,
}

/// Shared by the capture thread, the sink and the call loop. Every value is
/// `average/worst` over the window, in milliseconds.
pub struct ShareLoad {
    state: Mutex<State>,
}

impl ShareLoad {
    pub fn new() -> Arc<Self> {
        Arc::new(ShareLoad {
            state: Mutex::new(State {
                window: Window::starting(Instant::now()),
                last_frame: None,
            }),
        })
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        if let Ok(mut state) = self.state.lock() {
            change(&mut state);
        }
    }

    /// A frame reached the sink. The gap since the previous one shows freezes.
    pub fn note_frame(&self, now: Instant, size: (u32, u32), path: FramePath) {
        self.update(|state| {
            if let Some(last) = state.last_frame {
                state.window.gap.record(now.saturating_duration_since(last));
            }
            state.last_frame = Some(now);
            state.window.frames += 1;
            state.window.size = Some(size);
            state.window.path = Some(path);
        });
    }

    /// Time the capturer took to produce a frame, without our own callback.
    pub fn record_grab(&self, elapsed: Duration) {
        self.update(|state| state.window.grab.record(elapsed));
    }

    /// Pixel conversion done on the CPU (BGRA to I420, or NV12 to I420).
    pub fn record_convert(&self, elapsed: Duration) {
        self.update(|state| state.window.convert.record(elapsed));
    }

    /// The whole `LocalSink::push`, preview included.
    pub fn record_push(&self, elapsed: Duration) {
        self.update(|state| state.window.push.record(elapsed));
    }

    /// The hand-over to the WebRTC video source.
    pub fn record_handoff(&self, elapsed: Duration) {
        self.update(|state| state.window.handoff.record(elapsed));
    }

    /// Building and publishing the picture of the local preview.
    pub fn record_preview(&self, elapsed: Duration) {
        self.update(|state| state.window.preview.record(elapsed));
    }

    /// How long the call loop waited for `get_stats`, while a share runs.
    pub fn record_stats_poll(&self, elapsed: Duration) {
        self.update(|state| state.window.stats_poll.record(elapsed));
    }

    /// The line to log once a window is old enough and has something to say.
    pub fn take_report(&self, now: Instant) -> Option<String> {
        let mut state = self.state.lock().ok()?;
        if now.saturating_duration_since(state.window.started) < REPORT_EVERY {
            return None;
        }
        let report = (!state.window.is_empty()).then(|| state.window.describe(now));
        state.window = Window::starting(now);
        report
    }

    /// A new share starts from nothing, not from the previous one's averages.
    pub fn reset(&self, now: Instant) {
        self.update(|state| {
            state.window = Window::starting(now);
            state.last_frame = None;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn load_at(start: Instant) -> Arc<ShareLoad> {
        let load = ShareLoad::new();
        load.reset(start);
        load
    }

    #[test]
    fn a_stat_reports_its_average_and_its_worst_case() {
        let mut stat = RunningStat::default();
        stat.record(millis(2));
        stat.record(millis(4));
        stat.record(millis(12));
        assert_eq!(stat.describe(), "6.0/12.0ms");
        assert_eq!(RunningStat::default().describe(), "-");
    }

    #[test]
    fn nothing_is_reported_before_the_window_is_old_enough() {
        let start = Instant::now();
        let load = load_at(start);
        load.note_frame(start, (1920, 1080), FramePath::NativeNv12);
        assert_eq!(load.take_report(start + REPORT_EVERY - millis(1)), None);
        assert!(load.take_report(start + REPORT_EVERY).is_some());
    }

    #[test]
    fn a_report_carries_the_cadence_the_size_and_the_timings() {
        let start = Instant::now();
        let load = load_at(start);
        for frame in 0..100u64 {
            load.note_frame(
                start + millis(frame * 100),
                (1920, 1080),
                FramePath::CpuI420,
            );
            load.record_convert(millis(10));
            load.record_push(millis(3));
        }
        load.record_convert(millis(30));
        load.record_stats_poll(millis(40));
        let line = load.take_report(start + REPORT_EVERY).expect("a report");
        assert!(line.contains("path=cpu-i420"), "{line}");
        assert!(line.contains("size=1920x1080"), "{line}");
        assert!(line.contains("fps=10.0"), "{line}");
        assert!(line.contains("gap=100.0/100.0ms"), "{line}");
        assert!(line.contains("convert=10.2/30.0ms"), "{line}");
        assert!(line.contains("push=3.0/3.0ms"), "{line}");
        assert!(line.contains("stats-poll=40.0/40.0ms"), "{line}");
        assert!(line.contains("grab=-"), "{line}");
    }

    #[test]
    fn a_freeze_shows_in_the_worst_gap() {
        let start = Instant::now();
        let load = load_at(start);
        load.note_frame(start, (1280, 720), FramePath::NativeNv12);
        load.note_frame(start + millis(66), (1280, 720), FramePath::NativeNv12);
        load.note_frame(start + millis(9_466), (1280, 720), FramePath::NativeNv12);
        let line = load.take_report(start + REPORT_EVERY).expect("a report");
        assert!(line.contains("gap=4733.0/9400.0ms"), "{line}");
    }

    #[test]
    fn the_window_restarts_after_a_report() {
        let start = Instant::now();
        let load = load_at(start);
        load.note_frame(start, (1280, 720), FramePath::NativeNv12);
        assert!(load.take_report(start + REPORT_EVERY).is_some());
        assert_eq!(load.take_report(start + REPORT_EVERY * 2), None);
        load.note_frame(start + REPORT_EVERY * 2, (1280, 720), FramePath::NativeNv12);
        assert!(load.take_report(start + REPORT_EVERY * 3).is_some());
    }

    #[test]
    fn a_share_without_frames_still_reports_a_zero_cadence() {
        let start = Instant::now();
        let load = load_at(start);
        load.record_stats_poll(millis(25));
        let line = load.take_report(start + REPORT_EVERY).expect("a report");
        assert!(line.contains("fps=0.0"), "{line}");
        assert!(line.contains("size=-"), "{line}");
    }

    #[test]
    fn an_idle_window_stays_silent_and_a_reset_forgets_the_last_share() {
        let start = Instant::now();
        let load = load_at(start);
        assert_eq!(load.take_report(start + REPORT_EVERY), None);
        load.note_frame(start, (640, 360), FramePath::CpuI420);
        load.reset(start + millis(500));
        load.note_frame(start + millis(900), (640, 360), FramePath::CpuI420);
        let line = load.take_report(start + REPORT_EVERY + millis(500));
        assert!(
            line.is_some_and(|line| line.contains("gap=-")),
            "the gap must not span a reset"
        );
    }
}
