//! Lockstep playback pacer.
//!
//! Every bot in one playback run shares one monotonic frame clock. The clock
//! owns two things a per-bot scheduler cannot own correctly: which frame is
//! current (content position) and when that frame leaves the host (dispatch).
//!
//! The pacer is single threaded on purpose. Relative skew between bots is the
//! only skew a listener can hear, and a single dispatcher gives every bot the
//! same wake-up and the same clock read. Independent worker threads add
//! wake-up jitter and, when their catch-up decisions diverge, permanent
//! content drift.
//!
//! Error budget, from largest to smallest:
//!
//! * listener jitter buffer (`~ms`, outside our control)
//! * frame clock / catch-up (`0`, deterministic)
//! * dispatch burst (`~tens of us` for a rotating send order)
//! * encryption (`off the deadline`, done after the burst)
//!
//! The pacer never lets a bot run its own frame counter. When the host is
//! late, every bot advances to the frame the clock says, so no bot can drift
//! away from the others.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Opus frame length sent to Discord: 20 ms.
pub const FRAME_MILLIS: u64 = 20;

/// 48 kHz samples in one 20 ms frame.
pub const TIMESTAMP_INCREMENT: u32 = 960;

/// Longest single sleep slice. Keeps `stop` responsive during long waits.
pub const MAX_SLEEP_SLICE: Duration = Duration::from_millis(50);

/// Frames kept for percentile reporting. The rest only update the max.
pub const DEFAULT_SAMPLE_CAPACITY: usize = 4096;

/// Absolute mapping between frame index and monotonic time.
#[derive(Clone, Copy, Debug)]
pub struct FrameClock {
    anchor: Instant,
    frame: Duration,
}

impl FrameClock {
    pub fn new(anchor: Instant) -> Self {
        Self {
            anchor,
            frame: Duration::from_millis(FRAME_MILLIS),
        }
    }

    pub fn with_frame_duration(anchor: Instant, frame: Duration) -> Self {
        Self {
            anchor,
            frame: frame.max(Duration::from_micros(1)),
        }
    }

    pub fn anchor(&self) -> Instant {
        self.anchor
    }

    pub fn frame_duration(&self) -> Duration {
        self.frame
    }

    /// Deadline for the first byte of frame `frame`.
    pub fn deadline(&self, frame: usize) -> Instant {
        self.anchor + self.frame * frame.min(u32::MAX as usize) as u32
    }

    /// Frame the clock says is current at `now`. Zero before the anchor.
    pub fn frame_at(&self, now: Instant) -> usize {
        let elapsed = now.saturating_duration_since(self.anchor);
        let frame_ns = self.frame.as_nanos().max(1);
        (elapsed.as_nanos() / frame_ns) as usize
    }

    /// How late frame `frame` is at `now`.
    pub fn lateness(&self, frame: usize, now: Instant) -> Duration {
        now.saturating_duration_since(self.deadline(frame))
    }
}

/// Frames of silence sent before the first audible frame.
///
/// `lead` used to be dead air before the first packet. It is now pre-roll:
/// the stream is established and buffered for every listener before the
/// track starts, while the first audible frame still lands at the same
/// wall-clock instant as before.
pub fn preroll_frames(lead: Duration, frame: Duration, minimum_warmup: usize) -> usize {
    let frame_us = frame.as_micros().max(1);
    let lead_frames = (lead.as_micros() / frame_us) as usize;
    lead_frames + minimum_warmup
}

/// Frames to skip when the host is late, clamped to `limit`.
pub fn catch_up(current: usize, due: usize, limit: usize) -> usize {
    due.saturating_sub(current).min(limit)
}

/// Track position for a frame index. Pre-roll frames are silence and must
/// never consume track audio, even when the host stalls during pre-roll.
pub fn content_frame(frame: usize, warmup: usize) -> usize {
    frame.saturating_sub(warmup)
}

/// RTP sequence number for a frame.
pub fn sequence_for(base: u16, frame: usize) -> u16 {
    base.wrapping_add(frame as u16)
}

/// RTP timestamp for a frame.
pub fn timestamp_for(base: u32, frame: usize) -> u32 {
    base.wrapping_add((frame as u32).wrapping_mul(TIMESTAMP_INCREMENT))
}

/// One bot as seen by the pacer.
///
/// `prepare` may encrypt and must not block for long; the pacer calls it in
/// the slack after a dispatch burst. `advance` moves this bot's audio cursor
/// forward without sending, used when the shared clock outruns the pacer.
pub trait Sink: Send + Sync {
    fn prepare(&self, frame: usize, sequence: u16, timestamp: u32);
    fn dispatch(&self) -> bool;
    fn advance(&self, frames: usize);
    fn finished(&self) -> bool;
}

/// A named sink. The name is only used for diagnostics.
pub struct Target {
    pub name: String,
    pub sink: Arc<dyn Sink>,
}

impl Target {
    pub fn new(name: impl Into<String>, sink: Arc<dyn Sink>) -> Self {
        Self {
            name: name.into(),
            sink,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PacerConfig {
    pub frame_duration: Duration,
    pub maximum_frames: usize,
    /// Frames of pre-roll silence. They never advance the track position.
    pub warmup_frames: usize,
    pub base_sequence: u16,
    pub base_timestamp: u32,
    pub spin_window: Duration,
    pub sample_capacity: usize,
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self {
            frame_duration: Duration::from_millis(FRAME_MILLIS),
            maximum_frames: usize::MAX,
            warmup_frames: 0,
            base_sequence: 0,
            base_timestamp: 0,
            spin_window: Duration::from_micros(250),
            sample_capacity: DEFAULT_SAMPLE_CAPACITY,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SendReport {
    pub samples: usize,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

impl SendReport {
    pub fn summary(&self) -> String {
        format!(
            "p50={}us p95={}us p99={}us max={}us n={}",
            self.p50_us, self.p95_us, self.p99_us, self.max_us, self.samples
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub frames: usize,
    pub catch_up_frames: usize,
    pub elapsed: Duration,
    pub dispatch: Option<SendReport>,
    pub prepare: Option<SendReport>,
}

impl RunReport {
    pub fn summary(&self) -> String {
        let dispatch = self
            .dispatch
            .map(|report| report.summary())
            .unwrap_or_else(|| "n/a".to_string());
        let prepare = self
            .prepare
            .map(|report| format!("prep p99={}us max={}us", report.p99_us, report.max_us))
            .unwrap_or_else(|| "prep n/a".to_string());
        format!(
            "frames={} catch_up={} elapsed_ms={} send {} {}",
            self.frames,
            self.catch_up_frames,
            self.elapsed.as_millis(),
            dispatch,
            prepare
        )
    }
}

#[derive(Default)]
struct Samples {
    values: Vec<u32>,
    max: u32,
    capacity: usize,
}

impl Samples {
    fn new(capacity: usize) -> Self {
        Self {
            values: Vec::new(),
            max: 0,
            capacity,
        }
    }

    fn push(&mut self, micros: u32) {
        self.max = self.max.max(micros);
        if self.values.len() < self.capacity {
            self.values.push(micros);
        }
    }

    fn report(&self) -> Option<SendReport> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_unstable();
        let last = sorted.len() - 1;
        let at = |percent: usize| sorted[last * percent / 100];
        Some(SendReport {
            samples: sorted.len(),
            p50_us: at(50),
            p95_us: at(95),
            p99_us: at(99),
            max_us: self.max,
        })
    }
}

/// Single-threaded dispatcher. Create once per playback run.
pub struct Pacer {
    config: PacerConfig,
}

impl Pacer {
    pub fn new(config: PacerConfig) -> Self {
        Self { config }
    }

    /// Run until the sinks finish, `stopping` is set, or `maximum_frames`.
    pub fn run<F>(&self, targets: &[Target], stopping: &AtomicBool, log: F) -> RunReport
    where
        F: Fn(String),
    {
        let started = Instant::now();
        let mut report = RunReport::default();

        if targets.is_empty() || self.config.maximum_frames == 0 {
            report.elapsed = started.elapsed();
            return report;
        }

        let clock = FrameClock::with_frame_duration(started, self.config.frame_duration);
        let count = targets.len();
        let mut sends = Samples::new(self.config.sample_capacity);
        let mut prepares = Samples::new(self.config.sample_capacity);
        let mut stamps: Vec<Instant> = vec![started; count];
        let mut catch_up_total = 0usize;
        let mut frame = 0usize;

        prepare_all(targets, frame, &self.config);

        while frame < self.config.maximum_frames {
            if stopping.load(Ordering::Relaxed) {
                break;
            }
            if targets.iter().all(|target| target.sink.finished()) {
                break;
            }

            let now = wait_until(clock.deadline(frame), self.config.spin_window, stopping);
            if stopping.load(Ordering::Relaxed) {
                break;
            }

            let due = clock
                .frame_at(now)
                .min(self.config.maximum_frames.saturating_sub(1));
            if due > frame {
                let skip = catch_up(
                    content_frame(frame, self.config.warmup_frames),
                    content_frame(due, self.config.warmup_frames),
                    self.config.maximum_frames,
                );
                if skip > 0 {
                    catch_up_total += skip;
                    for target in targets {
                        target.sink.advance(skip);
                    }
                }
                frame = due;
                // The old prepared packet belongs to a frame the clock has
                // already passed; replace it before dispatch.
                prepare_all(targets, frame, &self.config);
            }

            let offset = frame % count;
            let burst = Instant::now();
            for step in 0..count {
                let index = (offset + step) % count;
                let _ = targets[index].sink.dispatch();
                stamps[index] = Instant::now();
            }

            if count > 1 {
                let first = stamps.iter().copied().min().unwrap_or(burst);
                let last = stamps.iter().copied().max().unwrap_or(burst);
                sends.push(duration_us(last.saturating_duration_since(first)));
            }

            report.frames += 1;

            frame += 1;
            if frame >= self.config.maximum_frames {
                break;
            }
            let prepare_started = Instant::now();
            prepare_all(targets, frame, &self.config);
            prepares.push(duration_us(prepare_started.elapsed()));
        }

        report.catch_up_frames = catch_up_total;
        report.dispatch = sends.report();
        report.prepare = prepares.report();
        report.elapsed = started.elapsed();

        if let Some(dispatch) = report.dispatch {
            log(format!(
                "[SYNC] lockstep {} send {}",
                targets.len(),
                dispatch.summary()
            ));
        }
        if let Some(prepare) = report.prepare {
            let budget = duration_us(self.config.frame_duration);
            if count > 1 && prepare.p99_us > budget / 2 {
                log(format!(
                    "[SYNC] prepare p99={}us is over half of the {}us frame budget; reduce bot count or prebuild packets",
                    prepare.p99_us, budget
                ));
            }
        }

        report
    }
}

fn prepare_all(targets: &[Target], frame: usize, config: &PacerConfig) {
    let sequence = sequence_for(config.base_sequence, frame);
    let timestamp = timestamp_for(config.base_timestamp, frame);
    for target in targets {
        target.sink.prepare(frame, sequence, timestamp);
    }
}

fn duration_us(value: Duration) -> u32 {
    value.as_micros().min(u32::MAX as u128) as u32
}

/// Sleep until close to `deadline`, then spin. Sleeping only the part that is
/// further away than `spin` keeps the wake-up error in the sub-millisecond
/// range without burning a core for the whole frame.
pub fn wait_until(deadline: Instant, spin: Duration, stopping: &AtomicBool) -> Instant {
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Instant::now();
        }
        let now = Instant::now();
        if now >= deadline {
            return now;
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining > spin {
            let sleep = remaining.saturating_sub(spin).min(MAX_SLEEP_SLICE);
            thread::sleep(sleep);
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn clock_keeps_absolute_grid() {
        let anchor = Instant::now() + Duration::from_millis(10);
        let clock = FrameClock::with_frame_duration(anchor, Duration::from_millis(20));
        assert_eq!(clock.deadline(0), anchor);
        assert_eq!(clock.deadline(1), anchor + Duration::from_millis(20));
        assert_eq!(
            clock.deadline(50),
            anchor + Duration::from_millis(50 * 20)
        );
        assert!(clock.deadline(7) > clock.deadline(6));
    }

    #[test]
    fn frame_at_rounds_down_and_starts_at_zero() {
        let anchor = Instant::now();
        let clock = FrameClock::with_frame_duration(anchor, Duration::from_millis(20));
        assert_eq!(clock.frame_at(anchor - Duration::from_millis(5)), 0);
        assert_eq!(clock.frame_at(anchor), 0);
        assert_eq!(clock.frame_at(anchor + Duration::from_millis(19)), 0);
        assert_eq!(clock.frame_at(anchor + Duration::from_millis(20)), 1);
        assert_eq!(clock.frame_at(anchor + Duration::from_millis(41)), 2);
    }

    #[test]
    fn preroll_keeps_the_old_start_time() {
        let frame = Duration::from_millis(20);
        let lead = Duration::from_millis(2_000);
        // 2 s pre-roll plus the old five warm-up frames.
        assert_eq!(preroll_frames(lead, frame, 5), 105);
        assert_eq!(preroll_frames(Duration::ZERO, frame, 0), 0);
        assert_eq!(preroll_frames(Duration::from_millis(9), frame, 1), 1);
    }

    #[test]
    fn catch_up_is_bounded() {
        assert_eq!(catch_up(10, 10, 100), 0);
        assert_eq!(catch_up(10, 13, 100), 3);
        assert_eq!(catch_up(10, 13, 2), 2);
        assert_eq!(catch_up(10, 9, 100), 0);
    }

    #[test]
    fn sequence_and_timestamp_advance_per_frame() {
        let base_sequence = 65_535u16;
        let base_timestamp = u32::MAX - 1_000;
        assert_eq!(sequence_for(base_sequence, 0), 65_535);
        assert_eq!(sequence_for(base_sequence, 1), 0);
        assert_eq!(timestamp_for(base_timestamp, 0), base_timestamp);
        assert_eq!(timestamp_for(base_timestamp, 1), base_timestamp.wrapping_add(960));
    }

    #[test]
    fn percentiles_are_ordered() {
        let mut samples = Samples::new(100);
        for value in 1..=100 {
            samples.push(value);
        }
        let report = samples.report().expect("report");
        assert_eq!(report.samples, 100);
        assert!(report.p50_us <= report.p95_us);
        assert!(report.p95_us <= report.p99_us);
        assert!(report.p99_us <= report.max_us);
        assert_eq!(report.max_us, 100);
    }

    #[test]
    fn samples_keep_capacity_but_track_max() {
        let mut samples = Samples::new(3);
        for value in [5, 1, 9, 40, 2] {
            samples.push(value);
        }
        let report = samples.report().expect("report");
        assert_eq!(report.samples, 3);
        assert_eq!(report.max_us, 40);
    }

    struct MockSink {
        id: usize,
        frames: Arc<Mutex<Vec<(usize, usize, u16, u32)>>>,
        dispatched: AtomicUsize,
        advanced: AtomicUsize,
        limit: usize,
        stall_at: Option<usize>,
    }

    impl MockSink {
        fn new(
            id: usize,
            frames: Arc<Mutex<Vec<(usize, usize, u16, u32)>>>,
            limit: usize,
            stall_at: Option<usize>,
        ) -> Self {
            Self {
                id,
                frames,
                dispatched: AtomicUsize::new(0),
                advanced: AtomicUsize::new(0),
                limit,
                stall_at,
            }
        }
    }

    impl Sink for MockSink {
        fn prepare(&self, frame: usize, sequence: u16, timestamp: u32) {
            self.frames
                .lock()
                .unwrap()
                .push((self.id, frame, sequence, timestamp));
        }

        fn dispatch(&self) -> bool {
            let previous = self.dispatched.fetch_add(1, Ordering::Relaxed);
            if self.stall_at == Some(previous) {
                thread::sleep(Duration::from_millis(12));
            }
            true
        }

        fn advance(&self, frames: usize) {
            self.advanced.fetch_add(frames, Ordering::Relaxed);
        }

        fn finished(&self) -> bool {
            self.dispatched.load(Ordering::Relaxed) >= self.limit
        }
    }

    fn run_mock(frame_duration: Duration, limit: usize, stall_at: Option<usize>) -> (RunReport, Vec<(usize, usize, u16, u32)>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let blocking = Arc::new(AtomicBool::new(false));
        let targets = vec![
            Target::new("a", Arc::new(MockSink::new(0, frames.clone(), limit, stall_at))),
            Target::new("b", Arc::new(MockSink::new(1, frames.clone(), limit, None))),
        ];
        let config = PacerConfig {
            frame_duration,
            maximum_frames: limit + 8,
            warmup_frames: 0,
            base_sequence: 100,
            base_timestamp: 5_000,
            spin_window: Duration::from_micros(50),
            sample_capacity: 64,
        };
        let report = Pacer::new(config).run(&targets, &blocking, |_| {});
        let recorded = frames.lock().unwrap().clone();
        (report, recorded)
    }

    #[test]
    fn pacer_gives_every_sink_the_same_frame_and_timestamp() {
        let (report, recorded) = run_mock(Duration::from_millis(5), 12, None);
        assert!(report.frames >= 6);
        assert!(report.dispatch.is_some());

        let frames_for = |id: usize| {
            recorded
                .iter()
                .filter(|entry| entry.0 == id)
                .map(|entry| entry.1)
                .collect::<Vec<_>>()
        };
        let first = frames_for(0);
        let second = frames_for(1);
        assert!(!first.is_empty());
        assert_eq!(first, second);
        for entry in &recorded {
            assert_eq!(entry.2, sequence_for(100, entry.1));
            assert_eq!(entry.3, timestamp_for(5_000, entry.1));
        }
    }

    #[test]
    fn pacer_advances_all_sinks_when_the_host_stalls() {
        let (report, recorded) = run_mock(Duration::from_millis(5), 20, Some(2));
        assert!(report.catch_up_frames > 0, "stall should trigger catch-up");

        // A stall burns frames for every sink at once; no sink may keep
        // preparing or sending a frame the other sink skipped.
        let frames_for = |id: usize| {
            recorded
                .iter()
                .filter(|entry| entry.0 == id)
                .map(|entry| entry.1)
                .collect::<Vec<_>>()
        };
        assert_eq!(frames_for(0), frames_for(1));
        for entry in recorded.iter() {
            assert_eq!(entry.2, sequence_for(100, entry.1));
            assert_eq!(entry.3, timestamp_for(5_000, entry.1));
        }
    }

    #[test]
    fn content_frame_never_goes_backwards() {
        assert_eq!(content_frame(0, 10), 0);
        assert_eq!(content_frame(9, 10), 0);
        assert_eq!(content_frame(10, 10), 0);
        assert_eq!(content_frame(11, 10), 1);
        assert_eq!(content_frame(5, 0), 5);
    }

    #[test]
    fn preroll_stall_does_not_consume_track_audio() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let first = Arc::new(MockSink::new(0, frames.clone(), 40, Some(1)));
        let second = Arc::new(MockSink::new(1, frames.clone(), 40, None));
        let targets = vec![
            Target::new("a", first.clone()),
            Target::new("b", second.clone()),
        ];
        let config = PacerConfig {
            frame_duration: Duration::from_millis(5),
            maximum_frames: 40,
            warmup_frames: 500,
            base_sequence: 0,
            base_timestamp: 0,
            spin_window: Duration::from_micros(50),
            sample_capacity: 16,
        };

        let stopping = AtomicBool::new(false);
        let report = Pacer::new(config).run(&targets, &stopping, |_| {});

        assert!(report.frames >= 10);
        // The 12 ms stall lands inside the 2.5 s of pre-roll, so the track
        // cursor must not move for either bot.
        assert_eq!(first.advanced.load(Ordering::Relaxed), 0);
        assert_eq!(second.advanced.load(Ordering::Relaxed), 0);
        // Both bots still recorded the same frames.
        let frames_for = |id: usize| {
            frames
                .lock()
                .unwrap()
                .iter()
                .filter(|entry| entry.0 == id)
                .map(|entry| entry.1)
                .collect::<Vec<_>>()
        };
        assert_eq!(frames_for(0), frames_for(1));
    }

    #[test]
    fn stop_flag_returns_quickly() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(true));
        let targets = vec![Target::new(
            "a",
            Arc::new(MockSink::new(0, frames, usize::MAX, None)),
        )];
        let config = PacerConfig {
            frame_duration: Duration::from_millis(5),
            ..PacerConfig::default()
        };
        let started = Instant::now();
        let report = Pacer::new(config).run(&targets, &stopping, |_| {});
        assert_eq!(report.frames, 0);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn wait_until_returns_after_the_deadline() {
        let stopping = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(3);
        let now = wait_until(deadline, Duration::from_micros(200), &stopping);
        assert!(now >= deadline);
    }

    #[test]
    fn empty_targets_report_is_empty() {
        let stopping = AtomicBool::new(false);
        let report = Pacer::new(PacerConfig::default()).run(&[], &stopping, |_| {});
        assert_eq!(report.frames, 0);
        assert!(report.dispatch.is_none());
    }
}
