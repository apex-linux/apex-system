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
    ApexError, BUS_NAME, Change, Channel, Deployment, ErrorKind, OBJECT_PATH, State, Update1Proxy,
};
use apexctl::service::DbusService;
use apexctl::update;

const BOOTED_DIGEST: &str =
    "sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";
const DOWNLOADED_DIGEST: &str =
    "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";
const STAGED_DIGEST: &str =
    "sha256:c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2";

struct PrivateBus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
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
            .build()
            .await
            .expect("client connection")
    }

    async fn serve(&self, mock: Mock) -> Connection {
        connection::Builder::address(self.address.as_str())
            .name(BUS_NAME)
            .serve_at(OBJECT_PATH, mock)
            .build()
            .await
            .expect("server connection")
    }

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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Shared {
    state: State,
    progress: f64,
    staged: Deployment,
    health_failure: String,
    download_denial: Option<String>,
    download_duration: Duration,
    automatic_error: Option<(ErrorKind, String)>,
    check_error: Option<(ErrorKind, String)>,
    changes_error: Option<(ErrorKind, String)>,
    /// `Member(argument)`, and whether interactive authorization was allowed.
    calls: Vec<(String, bool)>,
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
            health_failure: String::new(),
            download_denial: None,
            download_duration: Duration::ZERO,
            automatic_error: None,
            check_error: None,
            changes_error: None,
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
            .contains(Flags::ALLOW_INTERACTIVE_AUTH);
        self.lock().calls.push((call, interactive));
    }

    fn calls(&self) -> Vec<(String, bool)> {
        self.lock().calls.clone()
    }
}

fn booted() -> Deployment {
    Deployment {
        image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
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

fn update1_error(error: zbus::Error) -> ApexError {
    ApexError::from(error).into_contract()
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    reason = "zbus hands the header over by value, handlers keep the contract's Result shape, \
              and constant property getters must still be methods"
)]
#[zbus::interface(name = "io.github.apex_linux.Apex.Update1", crate = "apex_dbus::zbus")]
impl Mock {
    fn check(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        self.record(&header, "Check".to_owned());
        match self.lock().check_error.clone() {
            Some((kind, message)) => Err(ApexError::new(kind, message)),
            None => Ok(()),
        }
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

    /// Signals the change before it replies, like the daemon.
    async fn download(
        &self,
        override_soft_gates: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ApexError> {
        self.record(&header, format!("Download({override_soft_gates})"));
        let download_duration = {
            let shared = self.lock();
            if let Some(reason) = &shared.download_denial {
                return Err(ApexError::PolicyDenied(reason.clone()));
            }
            shared.download_duration
        };
        {
            let mut shared = self.lock();
            shared.state = State::Downloading;
            shared.progress = 0.5;
        }
        self.state_changed(&emitter).await.map_err(update1_error)?;
        self.progress_changed(&emitter)
            .await
            .map_err(update1_error)?;
        tokio::time::sleep(download_duration).await;
        {
            let mut shared = self.lock();
            shared.state = State::Staged;
            shared.progress = -1.0;
            shared.staged = Deployment {
                image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
                digest: Some(DOWNLOADED_DIGEST.to_owned()),
                version: Some("45.20260925.7".to_owned()),
                download_only: Some(true),
                ..Deployment::default()
            };
        }
        self.state_changed(&emitter).await.map_err(update1_error)?;
        self.progress_changed(&emitter)
            .await
            .map_err(update1_error)?;
        self.staged_changed(&emitter).await.map_err(update1_error)
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
            .map_err(update1_error)?;
        self.state_changed(&emitter).await.map_err(update1_error)
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
            image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
            digest: Some(STAGED_DIGEST.to_owned()),
            version: Some("45.20260925.118".to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        };
    }
    let _server = bus.serve(mock.clone()).await;

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

    let (code, stdout, stderr) = bus.apexctl(&["update", "download"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        "Downloaded 45.20260925.7 (sha256:5f70bf18a086). It stays locked until you apply it.\n\
         Use it at the next restart with: apexctl update apply\n"
    );

    mock.lock().download_denial = Some("Not enough free space in /sysroot".to_owned());
    let (code, _, stderr) = bus.apexctl(&["update", "download", "--force"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert_eq!(
        stderr,
        "apexctl: not now: Not enough free space in /sysroot\n"
    );

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
            ("Download(false)".to_owned(), true),
            ("Download(true)".to_owned(), true),
            ("AcknowledgeHealthFailure".to_owned(), true),
        ]
    );
}

#[tokio::test]
async fn the_progress_loop_polls_while_a_download_runs() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let mock = Mock::new();
    // Longer than two 500 ms progress ticks.
    mock.lock().download_duration = Duration::from_millis(1_200);
    let _server = bus.serve(mock.clone()).await;
    let client = bus.connect().await;
    // Without a property cache every read is a call the mock sees.
    let proxy = Update1Proxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .expect("proxy");
    let service = DbusService::from_proxy(proxy, true);

    let out = update::download(&service, false).await.expect("download");
    assert!(out.starts_with("Downloaded 45.20260925.7"), "{out}");
    let reads = mock.lock().state_reads.clone();
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

    let (code, stdout, stderr) = bus.apexctl(&["update", "check"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(
        stdout,
        "Not checked: an update is being downloaded now.\n\
         Downloading  25 %\n\
         Check again when the download has finished; apexctl status shows its progress.\n"
    );

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
