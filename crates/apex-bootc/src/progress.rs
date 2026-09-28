//! bootc's `--progress-fd` JSON Lines, protocol 0.1.0 (`docs/src/progress-v0.schema.json`).
//! Progress never fails an update: malformed, unknown or oversized lines are ignored.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Stage {
    Pulling,
    Retrying,
    Importing,
    Staging,
    Other(String),
}

impl Stage {
    fn from_event(task: &str, id: &str) -> Self {
        match task {
            "pulling" if id == "pull-retry" => Self::Retrying,
            "pulling" => Self::Pulling,
            "importing" => Self::Importing,
            "staging" => Self::Staging,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Pulling => "pulling",
            Self::Retrying => "retrying",
            Self::Importing => "importing",
            Self::Staging => "staging",
            Self::Other(task) => task,
        }
    }

    fn overall_span(&self) -> Option<(f64, f64)> {
        match self {
            Self::Pulling => Some((0.0, PULLING_END)),
            Self::Importing => Some((PULLING_END, IMPORTING_END)),
            Self::Staging => Some((IMPORTING_END, 1.0)),
            Self::Retrying | Self::Other(_) => None,
        }
    }

    fn position(&self, fraction: Option<f64>) -> Option<f64> {
        let (start, end) = self.overall_span()?;
        let within = fraction.unwrap_or(0.0);
        Some((start + within * (end - start)).clamp(0.0, 1.0))
    }
}

/// Layer downloads dominate the wall time.
const PULLING_END: f64 = 0.90;
const IMPORTING_END: f64 = 0.95;

/// bootc leaves out the bytes already cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteProgress {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    /// bootc's English description, e.g. `Pulling Image: sha256:…`.
    pub label: String,
    /// `None` means "keep showing the last known value", not "restart".
    pub fraction: Option<f64>,
    pub bytes: Option<ByteProgress>,
    overall: Option<f64>,
}

impl Progress {
    /// Completion of the whole download; never decreases within one [`ProgressDecoder`] run.
    /// `None` means "keep showing the last value".
    pub fn overall(&self) -> Option<f64> {
        self.overall
    }
}

/// After a retry bootc counts `bytes` from 0 of the layers still missing, and the failed
/// attempt first sends the `importing` events of a finished pull (`retry_pull_operation`).
#[derive(Debug, Default)]
struct RunTracker {
    highest_shown: f64,
    pull_attempt_start: f64,
    pull_attempt_finished: bool,
}

impl RunTracker {
    fn observe(&mut self, stage: &Stage, fraction: Option<f64>) -> Option<f64> {
        let position = match stage {
            Stage::Pulling => {
                let within = fraction?;
                self.pull_attempt_finished = within >= 1.0;
                self.pull_attempt_start + within * (PULLING_END - self.pull_attempt_start)
            }
            Stage::Retrying => {
                self.pull_attempt_start = self.highest_shown.min(PULLING_END);
                self.pull_attempt_finished = false;
                return None;
            }
            // A failed attempt sends these before bootc retries: not progress yet.
            Stage::Importing if !self.pull_attempt_finished => return None,
            Stage::Importing | Stage::Staging | Stage::Other(_) => stage.position(fraction)?,
        };
        self.highest_shown = self.highest_shown.max(position.clamp(0.0, 1.0));
        Some(self.highest_shown)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RawEvent {
    Start {},
    #[serde(rename_all = "camelCase")]
    ProgressBytes {
        #[serde(default)]
        task: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        id: String,
        #[serde(default)]
        bytes: u64,
        #[serde(default)]
        bytes_total: u64,
        #[serde(default)]
        steps: u64,
        #[serde(default)]
        steps_total: u64,
    },
    #[serde(rename_all = "camelCase")]
    ProgressSteps {
        #[serde(default)]
        task: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        id: String,
        #[serde(default)]
        steps: u64,
        #[serde(default)]
        steps_total: u64,
    },
    #[serde(other)]
    Unknown,
}

/// A zero total means bootc does not know it.
fn ratio(done: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (done as f64 / total as f64).clamp(0.0, 1.0))
}

pub fn parse_line(line: &str) -> Option<Progress> {
    let event: RawEvent = serde_json::from_str(line.trim()).ok()?;
    match event {
        RawEvent::ProgressBytes {
            task,
            description,
            id,
            bytes,
            bytes_total,
            steps,
            steps_total,
        } => {
            let stage = Stage::from_event(&task, &id);
            let byte_progress = (bytes_total > 0).then_some(ByteProgress {
                done: bytes.min(bytes_total),
                total: bytes_total,
            });
            let fraction = if stage == Stage::Retrying {
                None
            } else {
                ratio(bytes, bytes_total).or_else(|| ratio(steps, steps_total))
            };
            Some(Progress {
                overall: stage.position(fraction),
                stage,
                label: description,
                fraction,
                bytes: byte_progress,
            })
        }
        RawEvent::ProgressSteps {
            task,
            description,
            id,
            steps,
            steps_total,
        } => {
            let stage = Stage::from_event(&task, &id);
            // For a retry, `steps` counts attempts, not completion.
            let fraction = if stage == Stage::Retrying {
                None
            } else {
                ratio(steps, steps_total)
            };
            Some(Progress {
                overall: stage.position(fraction),
                stage,
                label: description,
                fraction,
                bytes: None,
            })
        }
        RawEvent::Start {} | RawEvent::Unknown => None,
    }
}

/// One decoder per bootc run.
#[derive(Debug, Default)]
pub struct ProgressDecoder {
    line: Vec<u8>,
    overflowed: bool,
    run: RunTracker,
}

impl ProgressDecoder {
    /// A real event listing every layer as a subtask is a few tens of KiB.
    pub const MAX_LINE: usize = 1 << 20;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, mut chunk: &[u8]) -> Vec<Progress> {
        let mut out = Vec::new();
        while let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            self.append(&chunk[..pos]);
            if let Some(p) = self.take_line() {
                out.push(p);
            }
            chunk = &chunk[pos + 1..];
        }
        self.append(chunk);
        out
    }

    /// Parses a last line that has no newline.
    pub fn finish(&mut self) -> Option<Progress> {
        self.take_line()
    }

    fn append(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.line.len() + bytes.len() > Self::MAX_LINE {
            self.overflowed = true;
            self.line = Vec::new();
            return;
        }
        self.line.extend_from_slice(bytes);
    }

    fn take_line(&mut self) -> Option<Progress> {
        let line = std::mem::take(&mut self.line);
        let overflowed = std::mem::replace(&mut self.overflowed, false);
        if overflowed {
            return None;
        }
        let mut update = std::str::from_utf8(&line).ok().and_then(parse_line)?;
        update.overall = self.run.observe(&update.stage, update.fraction);
        Some(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BYTES: &str = r#"{"bytes":1048576,"bytesCached":0,"bytesTotal":4194304,"description":"Pulling Image: sha256:ab","id":"sha256:ab","steps":1,"stepsCached":0,"stepsTotal":5,"subtasks":[{"bytes":1048576,"bytesCached":0,"bytesTotal":2097152,"description":"Layer","id":"sha256:cd","subtask":"ostree_chunk"}],"task":"pulling","type":"ProgressBytes"}"#;

    #[test]
    fn bytes_event() {
        let p = parse_line(BYTES).unwrap();
        assert_eq!(p.stage, Stage::Pulling);
        assert_eq!(p.label, "Pulling Image: sha256:ab");
        assert_eq!(p.fraction, Some(0.25));
        assert_eq!(
            p.bytes,
            Some(ByteProgress {
                done: 1_048_576,
                total: 4_194_304
            })
        );
        assert_eq!(p.overall(), Some(0.225));
    }

    #[test]
    fn zero_total_bytes_falls_back_to_steps() {
        let p = parse_line(
            r#"{"type":"ProgressBytes","task":"pulling","description":"d","id":"i","bytes":0,"bytesTotal":0,"steps":2,"stepsTotal":4,"subtasks":[]}"#,
        )
        .unwrap();
        assert_eq!(p.fraction, Some(0.5));
        assert_eq!(p.bytes, None);
    }

    #[test]
    fn steps_events() {
        let staging = parse_line(
            r#"{"description":"Deploying Image","id":"sha256:ab","steps":1,"stepsCached":0,"stepsTotal":3,"subtasks":[],"task":"staging","type":"ProgressSteps"}"#,
        )
        .unwrap();
        assert_eq!(staging.stage, Stage::Staging);
        assert_eq!(staging.fraction, Some(1.0 / 3.0));
        let retry = parse_line(
            r#"{"description":"Retrying pull (attempt 2/5)","id":"pull-retry","steps":2,"stepsCached":0,"stepsTotal":5,"subtasks":[],"task":"pulling","type":"ProgressSteps"}"#,
        )
        .unwrap();
        assert_eq!(retry.stage, Stage::Retrying);
        assert_eq!(retry.fraction, None);
        assert_eq!(retry.overall(), None);
    }

    #[test]
    fn ignored_lines() {
        for line in [
            r#"{"type":"Start","version":"0.1.0"}"#,
            r#"{"type":"SomethingNew","x":1}"#,
            r#"{"task":"pulling"}"#,
            r#"{"type":"ProgressBytes","bytes":-1}"#,
            "",
            "not json",
            "[]",
        ] {
            assert_eq!(parse_line(line), None, "{line}");
        }
    }

    #[test]
    fn unknown_task_is_other() {
        let p =
            parse_line(r#"{"type":"ProgressSteps","task":"verifying","steps":1,"stepsTotal":2}"#)
                .unwrap();
        assert_eq!(p.stage, Stage::Other("verifying".into()));
        assert_eq!(p.stage.as_str(), "verifying");
        assert_eq!(p.overall(), None);
    }

    #[test]
    fn decoder_handles_split_lines_and_overflow() {
        let mut d = ProgressDecoder::new();
        let (a, b) = BYTES.split_at(40);
        assert!(d.push(a.as_bytes()).is_empty());
        assert_eq!(d.push(format!("{b}\n").as_bytes()).len(), 1);
        let huge = vec![b'x'; ProgressDecoder::MAX_LINE + 1];
        assert!(d.push(&huge).is_empty());
        assert!(d.push(b"more garbage\n").is_empty());
        assert_eq!(d.push(format!("{BYTES}\n").as_bytes()).len(), 1);
        assert!(d.push(BYTES.as_bytes()).is_empty());
        assert!(d.finish().is_some());
        assert!(d.finish().is_none());
    }

    fn pulling(done: u64, total: u64) -> String {
        format!(
            r#"{{"bytes":{done},"bytesCached":0,"bytesTotal":{total},"description":"Pulling Image: sha256:ab","id":"sha256:ab","steps":0,"stepsCached":0,"stepsTotal":1,"subtasks":[],"task":"pulling","type":"ProgressBytes"}}"#
        )
    }

    fn steps(task: &str, id: &str, steps: u64, total: u64) -> String {
        format!(
            r#"{{"description":"d","id":"{id}","steps":{steps},"stepsCached":0,"stepsTotal":{total},"subtasks":[],"task":"{task}","type":"ProgressSteps"}}"#
        )
    }

    fn retry() -> String {
        steps("pulling", "pull-retry", 1, 3)
    }

    fn run(lines: &[String]) -> Vec<Option<f64>> {
        let mut d = ProgressDecoder::new();
        let stream = lines.join("\n") + "\n";
        d.push(stream.as_bytes())
            .iter()
            .map(|p| p.overall().map(|o| (o * 1000.0).round() / 1000.0))
            .collect()
    }

    #[test]
    fn a_retried_pull_fills_the_rest_of_the_pulling_span() {
        let lines = [
            pulling(0, 300),
            pulling(160, 300),
            steps("importing", "sha256:ab", 0, 1),
            steps("importing", "sha256:ab", 1, 1),
            retry(),
            pulling(0, 200),
            pulling(100, 200),
            pulling(200, 200),
            steps("importing", "sha256:ab", 0, 1),
            steps("importing", "sha256:ab", 1, 1),
            steps("staging", "sha256:ab", 3, 3),
        ];
        assert_eq!(
            run(&lines),
            [
                Some(0.0),
                Some(0.48),
                None,
                None,
                None,
                Some(0.48),
                Some(0.69),
                Some(0.9),
                Some(0.9),
                Some(0.95),
                Some(1.0),
            ]
        );
        let single: Vec<_> = lines
            .iter()
            .map(|l| parse_line(l).and_then(|p| p.overall()))
            .collect();
        assert_eq!(single[3], Some(0.95));
        assert_eq!(single[5], Some(0.0));
    }

    #[test]
    fn a_second_retry_rebases_again_and_never_overshoots() {
        let lines = [
            pulling(50, 100),
            retry(),
            pulling(50, 100),
            retry(),
            pulling(0, 10),
            pulling(10, 10),
        ];
        assert_eq!(
            run(&lines),
            [Some(0.45), None, Some(0.675), None, Some(0.675), Some(0.9)]
        );
    }

    #[test]
    fn importing_counts_only_after_a_finished_pull() {
        let all_layers_cached = [
            steps("importing", "sha256:ab", 1, 1),
            steps("staging", "sha256:ab", 0, 3),
        ];
        assert_eq!(run(&all_layers_cached), [None, Some(0.95)]);
        // Actual blob sizes can differ from the manifest's, so a byte count can stop short.
        let short = [
            pulling(99, 100),
            steps("importing", "sha256:ab", 1, 1),
            steps("staging", "sha256:ab", 1, 3),
        ];
        assert_eq!(run(&short), [Some(0.891), None, Some(0.967)]);
    }

    #[test]
    fn pulling_without_totals_keeps_the_last_value() {
        let no_totals = r#"{"type":"ProgressBytes","task":"pulling","description":"d","id":"i","bytes":0,"bytesTotal":0,"steps":0,"stepsTotal":0,"subtasks":[]}"#;
        let lines = [pulling(50, 100), no_totals.to_owned()];
        assert_eq!(run(&lines), [Some(0.45), None]);
        assert_eq!(parse_line(no_totals).and_then(|p| p.overall()), Some(0.0));
    }

    proptest! {
        #[test]
        fn a_run_never_goes_backwards(events in proptest::collection::vec((0u8..6, 0u64..120, 1u64..120), 0..40)) {
            let lines: Vec<String> = events
                .into_iter()
                .map(|(kind, a, b)| match kind {
                    0 | 1 => pulling(a.min(b), b),
                    2 => retry(),
                    3 => steps("importing", "sha256:ab", a % 2, 1),
                    4 => steps("staging", "sha256:ab", a % 4, 3),
                    _ => steps("verifying", "x", a, b),
                })
                .collect();
            let mut last = 0.0;
            for overall in run(&lines).into_iter().flatten() {
                prop_assert!((0.0..=1.0).contains(&overall));
                prop_assert!(overall >= last, "{overall} < {last}");
                last = overall;
            }
        }

        #[test]
        fn chunking_does_not_change_the_result(cuts in proptest::collection::vec(0usize..2000, 0..20)) {
            let stream = format!("{{\"type\":\"Start\",\"version\":\"0.1.0\"}}\n{BYTES}\ngarbage\n{BYTES}\n");
            let bytes = stream.as_bytes();
            let mut whole = ProgressDecoder::new();
            let expected = whole.push(bytes);
            prop_assert_eq!(expected.len(), 2);
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (bytes.len() + 1)).collect();
            cuts.sort_unstable();
            let mut d = ProgressDecoder::new();
            let mut got = Vec::new();
            let mut start = 0;
            for c in cuts.into_iter().chain([bytes.len()]) {
                got.extend(d.push(&bytes[start..c]));
                start = c;
            }
            prop_assert_eq!(got, expected);
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_fractions_stay_in_range(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut d = ProgressDecoder::new();
            for p in d.push(&data).into_iter().chain(d.finish()) {
                if let Some(f) = p.fraction { prop_assert!((0.0..=1.0).contains(&f)); }
                if let Some(o) = p.overall() { prop_assert!((0.0..=1.0).contains(&o)); }
            }
        }

        #[test]
        fn any_counts_give_a_fraction_in_range(bytes in any::<u64>(), total in any::<u64>(), steps in any::<u64>(), steps_total in any::<u64>()) {
            let line = format!(r#"{{"type":"ProgressBytes","task":"pulling","description":"","id":"x","bytes":{bytes},"bytesTotal":{total},"steps":{steps},"stepsTotal":{steps_total},"subtasks":[]}}"#);
            let p = parse_line(&line).unwrap();
            if let Some(f) = p.fraction { prop_assert!((0.0..=1.0).contains(&f)); }
            if let Some(b) = p.bytes { prop_assert!(b.done <= b.total); }
            let o = p.overall().unwrap();
            prop_assert!((0.0..=0.9).contains(&o));
        }
    }
}
