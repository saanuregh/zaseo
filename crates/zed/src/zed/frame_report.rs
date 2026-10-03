//! A dev-only frame report for measuring a real window: set `ZASEO_FRAME_REPORT=<path>` and
//! the app writes a summary of its frames there every few seconds.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{App, FrameEvent, FrameTimingCollector, WindowId};

const REPORT_INTERVAL: Duration = Duration::from_secs(5);
const FRAME_BUDGET: Duration = Duration::from_micros(16_667);
/// Samples kept per measure, so a report left on for hours stays small.
const MAX_SAMPLES: usize = 100_000;

/// Starts the report when `ZASEO_FRAME_REPORT` names a file. The file is rewritten every
/// [`REPORT_INTERVAL`] rather than on quit, because a window stopped with a signal never quits.
pub fn init(cx: &mut App) {
    let Some(path) = std::env::var_os("ZASEO_FRAME_REPORT").map(PathBuf::from) else {
        return;
    };
    gpui::set_trace_enabled(true);
    let started = Instant::now();
    let mut collector = FrameTimingCollector::new();
    let mut samples = FrameSamples::default();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(REPORT_INTERVAL).await;
            samples.add(collector.collect_unseen());
            let report = frame_summary(started.elapsed(), &samples);
            let path = path.clone();
            let written = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, report) })
                .await;
            if let Err(error) = written {
                log::error!("Could not write the frame report: {error}");
            }
        }
    })
    .detach();
}

#[derive(Default)]
struct FrameSamples {
    draws: VecDeque<Duration>,
    /// From a window's first change to the frame showing it being handed to the platform.
    change_to_present: VecDeque<Duration>,
    /// Between frames presented while something animates; the platform paces these, so a
    /// GPU or compositor falling behind lengthens them.
    animation_intervals: VecDeque<Duration>,
    /// When each window's last drawn frame first changed, until it is presented.
    pending_changes: HashMap<WindowId, Instant>,
}

impl FrameSamples {
    fn add(&mut self, events: Vec<FrameEvent>) {
        for event in events {
            match event {
                FrameEvent::Draw(frame) => {
                    push(&mut self.draws, frame.draw_end - frame.draw_start);
                    if let Some(dirty_at) = frame.dirty_at {
                        self.pending_changes.insert(frame.window_id, dirty_at);
                    }
                }
                FrameEvent::Present(present) => {
                    if let Some(dirty_at) = self.pending_changes.remove(&present.window_id) {
                        push(
                            &mut self.change_to_present,
                            present.present_end.saturating_duration_since(dirty_at),
                        );
                    }
                    if let Some(interval) = present.animation_interval {
                        push(&mut self.animation_intervals, interval);
                    }
                }
            }
        }
    }
}

fn push(samples: &mut VecDeque<Duration>, sample: Duration) {
    if samples.len() == MAX_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(sample);
}

fn frame_summary(elapsed: Duration, samples: &FrameSamples) -> String {
    let mut summary = format!("Zaseo frame report over {} s\n", elapsed.as_secs());
    for (name, values) in [
        ("draw (CPU)", &samples.draws),
        ("change to present", &samples.change_to_present),
        (
            "animation frame interval (GPU and compositor pacing)",
            &samples.animation_intervals,
        ),
    ] {
        summary.push_str(name);
        summary.push_str(": ");
        summary.push_str(&measure_summary(values));
        summary.push('\n');
    }
    summary
}

fn measure_summary(values: &VecDeque<Duration>) -> String {
    if values.is_empty() {
        return "no frames".to_owned();
    }
    let mut sorted = values.iter().copied().collect::<Vec<_>>();
    sorted.sort();
    // Nearest rank: the smallest value at least `fraction` of the samples don't exceed.
    let percentile = |fraction: f64| {
        let rank = (fraction * sorted.len() as f64).ceil() as usize;
        sorted
            .get(rank.saturating_sub(1))
            .copied()
            .unwrap_or_default()
    };
    let over_budget = sorted.iter().filter(|value| **value > FRAME_BUDGET).count();
    format!(
        "{} frames, p50 {}, p95 {}, p99 {}, max {}, {over_budget} over 16.7 ms",
        sorted.len(),
        milliseconds(percentile(0.5)),
        milliseconds(percentile(0.95)),
        milliseconds(percentile(0.99)),
        milliseconds(sorted.last().copied().unwrap_or_default()),
    )
}

fn milliseconds(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_summary_reports_percentiles_and_overruns() {
        let millis = |values: &[u64]| {
            values
                .iter()
                .map(|value| Duration::from_millis(*value))
                .collect::<VecDeque<_>>()
        };
        let samples = FrameSamples {
            draws: millis(&[1, 2, 3, 4, 20]),
            change_to_present: millis(&[2, 3, 30]),
            animation_intervals: millis(&[16, 17, 16]),
            ..FrameSamples::default()
        };
        let summary = frame_summary(Duration::from_secs(60), &samples);
        assert_eq!(
            summary,
            "Zaseo frame report over 60 s\n\
             draw (CPU): 5 frames, p50 3.0 ms, p95 20.0 ms, p99 20.0 ms, max 20.0 ms, 1 over 16.7 ms\n\
             change to present: 3 frames, p50 3.0 ms, p95 30.0 ms, p99 30.0 ms, max 30.0 ms, 1 over 16.7 ms\n\
             animation frame interval (GPU and compositor pacing): 3 frames, p50 16.0 ms, p95 17.0 ms, p99 17.0 ms, max 17.0 ms, 1 over 16.7 ms\n"
        );
        assert_eq!(
            frame_summary(Duration::from_secs(5), &FrameSamples::default()),
            "Zaseo frame report over 5 s\n\
             draw (CPU): no frames\n\
             change to present: no frames\n\
             animation frame interval (GPU and compositor pacing): no frames\n"
        );
    }
}
