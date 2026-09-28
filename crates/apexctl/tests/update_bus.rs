//! The update commands against a mock `apex-updated` on a private D-Bus daemon.
//!
//! Each test starts its own `dbus-daemon` (a child process listening in a temporary
//! directory) and serves `Mock`, the part of Update1 these tests need, under the contract's
//! bus name. The unit tests of `update` use an in-memory fake; these check what that fake
//! cannot:
//!
//! - the `a(sssss)` reply of `GetChanges` decoded and printed by the real binary;
//! - the channel string and the interactive-authorization flag on the wire, and the exit
//!   codes of `update channel`, `update acknowledge` and `update automatic` from the real
//!   binary, which reaches the mock through `DBUS_SYSTEM_BUS_ADDRESS`;
//! - `DbusService`'s progress loop polling the service while a long call is pending;
//! - how the binary reports `Busy` from `Check` (exit 0) and `Busy` or `Internal` from
//!   `GetChanges` before the service knows the system's state (exit 1), and what
//!   `update discard` prints when the re-lock staged a newer build.
//!
//! They need `dbus-daemon` on `PATH` (Fedora package `dbus-daemon`). Without it they
//! fail, so that CI cannot report them green without running them; set
//! `APEXCTL_TEST_SKIP_BUS=1` to skip them on purpose (they then print why and pass).

#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use apex_dbus::zbus::message::{Flags, Header};
use apex_dbus::zbus::object_server::SignalEmitter;
use apex_dbus::zbus::proxy::CacheProperties;
use apex_dbus::zbus::{self, Connection, connection};
use apex_dbus::{
    ApexError, BUS_NAME, Change, Channel, Deployment, ErrorKind, OBJECT_PATH, State,
    UnknownValueError, Update1Proxy,
};
use apexctl::service::DbusService;
use apexctl::update;

const BOOTED_DIGEST: &str =
    "sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";
const TESTING_DIGEST: &str =
    "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";
const STAGED_DIGEST: &str =
    "sha256:c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2";
const NEWER_DIGEST: &str =
    "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// A private message bus: `dbus-daemon` with a permissive configuration in a temporary
/// directory. It is killed when dropped.
struct PrivateBus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// Starts the daemon. Without one it fails the test, unless `APEXCTL_TEST_SKIP_BUS`
    /// is set: then it says why and returns `None`, and the test passes without running.
    fn start() -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )
        .expect("write bus.conf");
        let spawned = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) if std::env::var_os("APEXCTL_TEST_SKIP_BUS").is_some() => {
                eprintln!(
                    "SKIPPED (APEXCTL_TEST_SKIP_BUS is set): cannot start dbus-daemon ({error})"
                );
                return None;
            }
            Err(error) => panic!(
                "cannot start dbus-daemon: {error}. These tests need it (Fedora package \
                 dbus-daemon); set APEXCTL_TEST_SKIP_BUS=1 to skip them on purpose"
            ),
        };
        // The daemon prints its address once it listens.
        let mut address = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut address)
            .expect("read the bus address");
        let address = address.trim().to_owned();
        assert!(address.starts_with("unix:"), "dbus-daemon said {address:?}");
        Some(Self {
            child,
            address,
            _dir: dir,
        })
    }

    async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .expect("address")
            .build()
            .await
            .expect("client connection")
    }

    /// Serves `mock` at the contract path under the contract's bus name.
    async fn serve(&self, mock: Mock) -> Connection {
        connection::Builder::address(self.address.as_str())
            .expect("address")
            .name(BUS_NAME)
            .expect("bus name")
            .serve_at(OBJECT_PATH, mock)
            .expect("serve")
            .build()
            .await
            .expect("server connection")
    }

    /// Runs the `apexctl` binary against this bus: (exit code, stdout, stderr).
    async fn apexctl(&self, args: &[&str]) -> (i32, String, String) {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_apexctl"))
            .args(args)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &self.address)
            .output()
            .await
            .expect("apexctl runs");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // Best effort: the daemon may already be gone.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The mock's state, shared with the test.
#[derive(Debug)]
struct Shared {
    state: State,
    progress: f64,
    staged: Deployment,
    followed: Channel,
    health_failure: String,
    /// When set, `SwitchChannel` refuses with `PolicyDenied` and this reason.
    deny: Option<String>,
    /// How long `SwitchChannel` "downloads".
    switch_time: Duration,
    /// When set, `Automatic` fails with this error kind and message.
    automatic_error: Option<(ErrorKind, String)>,
    /// When set, `Check` fails with this error kind and message.
    check_error: Option<(ErrorKind, String)>,
    /// When set, `GetChanges` fails with this error kind and message.
    changes_error: Option<(ErrorKind, String)>,
    /// What `DiscardStaged` stages in place of the applied update (the re-lock pulled the
    /// channel tag, which moved on).
    relocked: Deployment,
    /// Method calls as `Member(argument)`, with whether interactive authorization was allowed.
    calls: Vec<(String, bool)>,
    /// The `State` value at each `State` read, in order.
    state_reads: Vec<State>,
}

#[derive(Debug, Clone)]
struct Mock(Arc<Mutex<Shared>>);

impl Mock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Shared {
            state: State::UpToDate,
            progress: -1.0,
            staged: Deployment::default(),
            followed: Channel::Stable,
            health_failure: String::new(),
            deny: None,
            switch_time: Duration::ZERO,
            automatic_error: None,
            check_error: None,
            changes_error: None,
            relocked: Deployment::default(),
            calls: Vec::new(),
            state_reads: Vec::new(),
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, header: &Header<'_>, call: String) {
        let interactive = header
            .primary()
            .flags()
            .contains(Flags::AllowInteractiveAuth);
        self.lock().calls.push((call, interactive));
    }

    fn calls(&self) -> Vec<(String, bool)> {
        self.lock().calls.clone()
    }
}

fn booted() -> Deployment {
    Deployment {
        image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
        digest: Some(BOOTED_DIGEST.to_owned()),
        version: Some("45.20260924.117".to_owned()),
        ..Deployment::default()
    }
}

fn change(name: &str, arch: &str, kind: &str, from: &str, to: &str) -> Change {
    (
        name.to_owned(),
        arch.to_owned(),
        kind.to_owned(),
        from.to_owned(),
        to.to_owned(),
    )
}

/// Turns a zbus failure inside a handler into a contract error.
fn contract(error: zbus::Error) -> ApexError {
    ApexError::from(error).into_contract()
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    reason = "zbus hands the header over by value, handlers keep the contract's Result shape, \
              and constant property getters must still be methods"
)]
#[zbus::interface(
    name = "io.github.sekirokenjii.Apex.Update1",
    crate = "apex_dbus::zbus"
)]
impl Mock {
    fn check(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        self.record(&header, "Check".to_owned());
        match self.lock().check_error.clone() {
            Some((kind, message)) => Err(ApexError::new(kind, message)),
            None => Ok(()),
        }
    }

    /// Locks the applied update again, replaced by `relocked`, and signals the change
    /// before it replies, like the daemon.
    async fn discard_staged(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ApexError> {
        self.record(&header, "DiscardStaged".to_owned());
        {
            let mut shared = self.lock();
            shared.staged = shared.relocked.clone();
            shared.state = State::Staged;
        }
        self.staged_changed(&emitter).await.map_err(contract)?;
        self.state_changed(&emitter).await.map_err(contract)
    }

    #[zbus(out_args("changes"))]
    fn get_changes(&self, #[zbus(header)] header: Header<'_>) -> Result<Vec<Change>, ApexError> {
        self.record(&header, "GetChanges".to_owned());
        if let Some((kind, message)) = self.lock().changes_error.clone() {
            return Err(ApexError::new(kind, message));
        }
        Ok(vec![
            change("glibc", "i686", "upgraded", "2.42-4.fc45", "2.42-5.fc45"),
            change("glibc", "x86_64", "upgraded", "2.42-4.fc45", "2.42-5.fc45"),
            change(
                "kernel",
                "x86_64",
                "upgraded",
                "7.2.7-300.fc45",
                "7.2.9-300.fc45",
            ),
        ])
    }

    /// Takes the raw string, like the daemon, so the test sees exactly what went on the wire.
    /// "Downloads" for `switch_time` in state `downloading`, then stages the channel's image.
    async fn switch_channel(
        &self,
        channel: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ApexError> {
        self.record(&header, format!("SwitchChannel({channel})"));
        let channel: Channel = channel
            .parse()
            .map_err(|error: UnknownValueError| ApexError::InvalidArgument(error.to_string()))?;
        let switch_time = {
            let shared = self.lock();
            if let Some(reason) = &shared.deny {
                return Err(ApexError::PolicyDenied(reason.clone()));
            }
            if shared.followed == channel {
                return Err(ApexError::NothingToDo(format!(
                    "This system already follows the {channel} channel."
                )));
            }
            shared.switch_time
        };
        {
            let mut shared = self.lock();
            shared.state = State::Downloading;
            shared.progress = 0.5;
        }
        self.state_changed(&emitter).await.map_err(contract)?;
        self.progress_changed(&emitter).await.map_err(contract)?;
        tokio::time::sleep(switch_time).await;
        {
            let mut shared = self.lock();
            shared.state = State::Staged;
            shared.progress = -1.0;
            shared.followed = channel;
            shared.staged = Deployment {
                image: Some(
                    match channel {
                        Channel::Stable => "ghcr.io/sekirokenjii/apex:45",
                        Channel::Testing => "ghcr.io/sekirokenjii/apex-testing:45",
                    }
                    .to_owned(),
                ),
                digest: Some(TESTING_DIGEST.to_owned()),
                version: Some("45.20260925.7".to_owned()),
                download_only: Some(true),
                ..Deployment::default()
            };
        }
        self.state_changed(&emitter).await.map_err(contract)?;
        self.progress_changed(&emitter).await.map_err(contract)?;
        self.staged_changed(&emitter).await.map_err(contract)
    }

    fn automatic(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        self.record(&header, "Automatic".to_owned());
        match self.lock().automatic_error.clone() {
            Some((kind, message)) => Err(ApexError::new(kind, message)),
            None => Ok(()),
        }
    }

    async fn acknowledge_health_failure(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ApexError> {
        self.record(&header, "AcknowledgeHealthFailure".to_owned());
        {
            let mut shared = self.lock();
            shared.health_failure.clear();
            shared.state = State::UpToDate;
        }
        self.health_failure_changed(&emitter)
            .await
            .map_err(contract)?;
        self.state_changed(&emitter).await.map_err(contract)
    }

    #[zbus(property)]
    fn state(&self) -> State {
        let mut shared = self.lock();
        let state = shared.state;
        shared.state_reads.push(state);
        state
    }

    #[zbus(property)]
    fn progress(&self) -> f64 {
        self.lock().progress
    }

    #[zbus(property)]
    fn booted(&self) -> Deployment {
        booted()
    }

    #[zbus(property)]
    fn staged(&self) -> Deployment {
        self.lock().staged.clone()
    }

    #[zbus(property)]
    fn rollback(&self) -> Deployment {
        Deployment::default()
    }

    #[zbus(property)]
    fn available(&self) -> Deployment {
        Deployment::default()
    }

    /// The booted image's channel: it does not change until a switched image boots.
    #[zbus(property)]
    fn channel(&self) -> Channel {
        Channel::Stable
    }

    #[zbus(property)]
    fn last_check(&self) -> u64 {
        0
    }

    #[zbus(property)]
    fn deferred_reason(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn last_error(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn stale(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn health_failure(&self) -> String {
        self.lock().health_failure.clone()
    }
}

#[tokio::test]
async fn the_binary_drives_the_new_update_commands_over_the_bus() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    {
        let mut shared = mock.lock();
        shared.health_failure = BOOTED_DIGEST.to_owned();
        shared.state = State::Staged;
        shared.staged = Deployment {
            image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
            digest: Some(STAGED_DIGEST.to_owned()),
            version: Some("45.20260925.118".to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        };
    }
    let _server = bus.serve(mock.clone()).await;

    // GetChanges' a(sssss): the arch reaches the list and the summary, under the staged
    // version and full digest, and above the command that applies exactly that digest.
    let (code, stdout, stderr) = bus.apexctl(&["update", "changes"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "Changes from 45.20260924.117 to 45.20260925.118 ({STAGED_DIGEST})\n\
             \n\
             Linux 7.2.7 → 7.2.9, 2 other packages\n\
             \n  upgraded    glibc.i686  2.42-4.fc45 → 2.42-5.fc45\
             \n  upgraded    glibc       2.42-4.fc45 → 2.42-5.fc45\
             \n  upgraded    kernel      7.2.7-300.fc45 → 7.2.9-300.fc45\n\
             \n\
             Apply exactly this update with: apexctl update apply --digest {STAGED_DIGEST}\n"
        )
    );

    // SwitchChannel gets the contract string, with interactive authorization allowed so
    // polkit can ask for the administrator password (auth_admin).
    let (code, stdout, stderr) = bus.apexctl(&["update", "channel", "testing"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        "Apex now follows the testing channel. Its image 45.20260925.7 (sha256:5f70bf18a086) \
         is downloaded and stays locked until you apply it.\n\
         To finish the switch, run: apexctl update apply\n\
         Then restart the computer; apexctl never restarts it by itself.\n\
         Testing images are signed with the CI key and have not been qualified for stable.\n"
    );
    let (code, stdout, stderr) = bus.apexctl(&["update", "channel", "testing"]).await;
    assert_eq!(code, 3, "already followed: {stderr}");
    assert!(stdout.is_empty());
    assert_eq!(
        stderr,
        "apexctl: This system already follows the testing channel.\n"
    );

    // A refusing gate exits 2, without the download-only --force advice.
    mock.lock().deny = Some("Not enough free space in /sysroot".to_owned());
    let (code, _, stderr) = bus.apexctl(&["update", "channel", "stable"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert_eq!(
        stderr,
        "apexctl: not now: Not enough free space in /sysroot\n"
    );

    // AcknowledgeHealthFailure clears the notice; a second run has nothing to clear.
    let (code, stdout, stderr) = bus.apexctl(&["update", "acknowledge"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("Acknowledged: update sha256:2c26b46b68ff failed its health checks."),
        "{stdout}"
    );
    assert!(
        stdout.contains("apexctl update rollback --mark-bad"),
        "{stdout}"
    );
    let (code, _, stderr) = bus.apexctl(&["update", "acknowledge"]).await;
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("nothing to acknowledge"), "{stderr}");

    assert_eq!(
        mock.calls(),
        [
            ("GetChanges".to_owned(), false),
            ("SwitchChannel(testing)".to_owned(), true),
            ("SwitchChannel(testing)".to_owned(), true),
            ("SwitchChannel(stable)".to_owned(), true),
            ("AcknowledgeHealthFailure".to_owned(), true),
        ]
    );
}

#[tokio::test]
async fn the_progress_loop_polls_while_a_switch_runs() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    // Longer than two 500 ms progress ticks.
    mock.lock().switch_time = Duration::from_millis(1_200);
    let _server = bus.serve(mock.clone()).await;
    let client = bus.connect().await;
    // Without a property cache every read is a call the mock sees.
    let proxy = Update1Proxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .expect("proxy");
    let service = DbusService::from_proxy(proxy, true);

    let out = update::channel(&service, Channel::Testing)
        .await
        .expect("switch");
    assert!(
        out.starts_with("Apex now follows the testing channel. Its image 45.20260925.7"),
        "{out}"
    );
    let reads = mock.lock().state_reads.clone();
    // The loop read the state while the call was pending and saw the download, and the
    // status read after the call saw the stage.
    assert!(reads.contains(&State::Downloading), "{reads:?}");
    assert_eq!(reads.last(), Some(&State::Staged), "{reads:?}");
}

#[tokio::test]
async fn the_timer_run_exits_0_when_it_only_has_to_wait() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    let _server = bus.serve(mock.clone()).await;

    // Another operation holds the daemon (a user download can take two hours), or the
    // registry is unreachable: a deferral, so apex-update-auto.service does not fail.
    for (error, why) in [
        (
            (ErrorKind::Busy, "Another update operation is running."),
            "another update operation is running",
        ),
        (
            (ErrorKind::Network, "no route to ghcr.io"),
            "the image registry cannot be reached",
        ),
    ] {
        mock.lock().automatic_error = Some((error.0, error.1.to_owned()));
        let (code, stdout, stderr) = bus.apexctl(&["update", "automatic"]).await;
        assert_eq!(code, 0, "{stderr}");
        assert!(
            stdout.starts_with(&format!("Automatic update run skipped: {why} (")),
            "{stdout}"
        );
    }

    // A real refusal still fails the unit, with root advice rather than polkit advice.
    mock.lock().automatic_error = Some((
        ErrorKind::NotAuthorized,
        "Only the system can call this method (caller uid 1000).".to_owned(),
    ));
    let (code, _, stderr) = bus.apexctl(&["update", "automatic"]).await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.ends_with("runs as root: sudo apexctl update automatic\n"),
        "{stderr}"
    );

    mock.lock().automatic_error = None;
    let (code, stdout, stderr) = bus.apexctl(&["update", "automatic"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("Automatic update run finished. Apex is up to date."),
        "{stdout}"
    );
}

#[tokio::test]
async fn busy_and_unknown_state_replies_are_reported_plainly() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    {
        let mut shared = mock.lock();
        shared.state = State::Downloading;
        shared.progress = 0.25;
        shared.check_error = Some((
            ErrorKind::Busy,
            "An update is being downloaded. Try again when the download has finished.".to_owned(),
        ));
    }
    let _server = bus.serve(mock.clone()).await;

    // Check while a download runs: nothing was checked, and that is not a failure.
    let (code, stdout, stderr) = bus.apexctl(&["update", "check"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(
        stdout,
        "Not checked: an update is being downloaded now.\n\
         Downloading  25 %\n\
         Check again when the download has finished; apexctl status shows its progress.\n"
    );

    // GetChanges before the service has read the system: Busy, then Internal once bootc
    // turned out to be unusable. Neither is "another update operation is running".
    {
        let mut shared = mock.lock();
        shared.state = State::Unknown;
        shared.progress = -1.0;
        shared.changes_error = Some((
            ErrorKind::Busy,
            "The update state is not known yet. Try again in a moment.".to_owned(),
        ));
    }
    let (code, stdout, stderr) = bus.apexctl(&["update", "changes"]).await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(
        stderr,
        "apexctl: Cannot list the package changes now. The update state is not known yet. Try \
         again in a moment.\n"
    );
    mock.lock().changes_error = Some((
        ErrorKind::Internal,
        "The update state could not be determined: bootc status failed".to_owned(),
    ));
    let (code, _, stderr) = bus.apexctl(&["update", "changes"]).await;
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr,
        "apexctl: internal error in apex-updated: cannot list the package changes: The update \
         state could not be determined: bootc status failed\n\
         The service's log may say more: journalctl -u apex-updated\n"
    );

    assert_eq!(
        mock.calls(),
        [
            ("Check".to_owned(), true),
            ("GetChanges".to_owned(), false),
            ("GetChanges".to_owned(), false),
        ]
    );
}

#[tokio::test]
async fn discard_names_the_build_the_relock_left_behind() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    {
        let mut shared = mock.lock();
        shared.state = State::ReadyForReboot;
        shared.staged = Deployment {
            image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
            digest: Some(STAGED_DIGEST.to_owned()),
            version: Some("45.20260925.118".to_owned()),
            download_only: Some(false),
            ..Deployment::default()
        };
        shared.relocked = Deployment {
            image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
            digest: Some(NEWER_DIGEST.to_owned()),
            version: Some("45.20260926.119".to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        };
    }
    let _server = bus.serve(mock.clone()).await;

    // The status read after the reply sees the new stage (the property cache is updated by
    // the PropertiesChanged signals the service sent before it replied).
    let (code, stdout, stderr) = bus.apexctl(&["update", "discard"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "45.20260925.118 will not be installed at the next restart.\n\
             The update channel has moved on: cancelling downloaded its current build, which \
             replaced it and is locked:\n\
             \x20 45.20260926.119  {NEWER_DIGEST}\n\
             Review it with: apexctl update changes\n\
             To install exactly this build later: apexctl update apply --digest {NEWER_DIGEST}\n\
             An automatic download may replace it with a newer build before then.\n"
        )
    );
    assert_eq!(mock.calls(), [("DiscardStaged".to_owned(), true)]);
}
