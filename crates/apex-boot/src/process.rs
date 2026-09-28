//! Running a helper program with a deadline.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error("cannot start {program}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("cannot wait for {program}")]
    Wait {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("{program} did not finish within {timeout:?} and was killed")]
    TimedOut { program: String, timeout: Duration },
}

/// Takes at most `timeout` plus [`crate::KILL_REAP_GRACE`]. Output is drained on its own
/// threads so a chatty child can never block on a full pipe.
pub(crate) fn run(command: &mut Command, timeout: Duration) -> Result<Finished, RunError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Spawn {
            program: program.clone(),
            source,
        })?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill(&mut child);
                return Err(RunError::TimedOut { program, timeout });
            }
            Ok(None) => thread::sleep(POLL),
            Err(source) => {
                kill(&mut child);
                return Err(RunError::Wait { program, source });
            }
        }
    };
    Ok(Finished {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

impl RunError {
    pub(crate) fn describe(&self) -> String {
        match std::error::Error::source(self) {
            Some(source) => format!("{self}: {source}"),
            None => self.to_string(),
        }
    }
}

fn kill(child: &mut Child) {
    kill_within(child, crate::KILL_REAP_GRACE);
}

/// Returns whether the child was reaped. One that is not stays a zombie until this
/// short-lived process exits, which is better than blocking the assessment on it.
fn kill_within(child: &mut Child, grace: Duration) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(None) | Err(_) => return false,
        }
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_output_and_status() {
        let done = run(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(done.status.code(), Some(3));
        assert_eq!(done.stdout, b"out\n");
        assert_eq!(done.stderr, b"err\n");
    }

    #[test]
    fn kills_after_timeout() {
        let start = Instant::now();
        let err = run(
            Command::new("/bin/sh").args(["-c", "exec sleep 30"]),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(matches!(err, RunError::TimedOut { .. }), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_killed_child_is_reaped_or_abandoned_within_the_grace() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        assert!(kill_within(&mut child, Duration::from_secs(10)));
        let start = Instant::now();
        let _ = kill_within(&mut child, Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_secs(5));
        // A zero grace stands in for a child stuck in uninterruptible sleep.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        let start = Instant::now();
        let reaped = kill_within(&mut child, Duration::ZERO);
        assert!(start.elapsed() < Duration::from_secs(1));
        if !reaped {
            let _ = child.wait();
        }
    }

    #[test]
    fn reports_spawn_failure() {
        let err = run(
            &mut Command::new("/nonexistent/apex-boot-test"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, RunError::Spawn { .. }), "{err}");
    }
}
