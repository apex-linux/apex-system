#![allow(
    dead_code,
    unreachable_pub,
    reason = "each test binary uses a different part of this module"
)]

use std::sync::Arc;

use apex_bootc::{
    BootEntry, BootEntryOstree, BootcCommand, FakeRunner, Host, HostSpec, HostStatus, HostType,
    ImageReference, ImageSignature, ImageStatus, Scripted,
};
use apex_update::{Intent, LastCheck, Manifest, Package, StateDir, StateStore};
use apex_updated::daemon::Daemon;
use apex_updated::engine::{Engine, EngineParts};
use apex_updated::queue::Op;
use apex_updated::testing::{FakeState, FakeSystem, SharedGrubWriter};
use tempfile::TempDir;

pub const BOOTED: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const STAGED: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";
pub const REMOTE: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
pub const ROLLBACK: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub const BOOTED_CSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
pub const STAGED_CSUM: &str = "2222222222222222222222222222222222222222222222222222222222222222";
pub const ROLLBACK_CSUM: &str = "3333333333333333333333333333333333333333333333333333333333333333";

pub const BOOTED_VERSION: &str = "45.20260920.1";
pub const STAGED_VERSION: &str = "45.20260924.3";
pub const OLDER_VERSION: &str = "45.20260918.1";

pub const STABLE_IMAGE: &str = "ghcr.io/apex-linux/apex:45";

pub const NOW: u64 = 1_790_300_000;

pub fn image(digest: &str, version: &str) -> ImageStatus {
    ImageStatus {
        image: ImageReference {
            image: STABLE_IMAGE.into(),
            transport: "registry".into(),
            signature: Some(ImageSignature::ContainerPolicy),
        },
        version: Some(version.into()),
        timestamp: Some("2026-09-24T03:12:45Z".into()),
        image_digest: digest.into(),
        architecture: "amd64".into(),
    }
}

pub fn entry(digest: &str, version: &str, checksum: &str) -> BootEntry {
    BootEntry {
        image: Some(image(digest, version)),
        ostree: Some(BootEntryOstree {
            stateroot: "default".into(),
            checksum: checksum.into(),
            deploy_serial: 0,
        }),
        ..BootEntry::default()
    }
}

pub fn booted() -> BootEntry {
    entry(BOOTED, BOOTED_VERSION, BOOTED_CSUM)
}

pub fn staged(download_only: bool) -> BootEntry {
    BootEntry {
        download_only,
        ..entry(STAGED, STAGED_VERSION, STAGED_CSUM)
    }
}

pub fn rollback() -> BootEntry {
    entry(ROLLBACK, OLDER_VERSION, ROLLBACK_CSUM)
}

pub fn host(booted: BootEntry, staged: Option<BootEntry>, rollback: Option<BootEntry>) -> Host {
    Host {
        api_version: "org.containers.bootc/v1".into(),
        kind: "BootcHost".into(),
        spec: HostSpec {
            image: staged
                .as_ref()
                .or(Some(&booted))
                .and_then(|e| e.image.as_ref())
                .map(|i| i.image.clone()),
            ..HostSpec::default()
        },
        status: HostStatus {
            booted: Some(booted),
            staged,
            rollback,
            host_type: Some(HostType::BootcHost),
            ..HostStatus::default()
        },
        ..Host::default()
    }
}

pub fn status(host: &Host) -> Scripted {
    Scripted::new(BootcCommand::Status).stdout(serde_json::to_vec(host).expect("status JSON"))
}

pub fn package(name: &str, version: &str, release: &str) -> Package {
    Package {
        name: name.into(),
        epoch: 0,
        version: version.into(),
        release: release.into(),
        arch: "x86_64".into(),
    }
}

pub fn manifest(version: &str, packages: Vec<Package>) -> Manifest {
    Manifest {
        schema: 1,
        name: "apex".into(),
        version: version.into(),
        fedora: 45,
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        base_image: "quay.io/fedora/fedora-bootc@sha256:00".into(),
        degraded: vec!["audio".into()],
        held: vec!["nautilus".into(), "gnome-shell".into(), "nautilus".into()],
        packages,
    }
}

pub fn fake_state() -> FakeState {
    let manifests = [
        (
            STAGED_CSUM.to_owned(),
            manifest(
                STAGED_VERSION,
                vec![
                    package("kernel", "6.17.2", "300.fc45"),
                    package("mutter", "51.0", "1.fc45"),
                ],
            ),
        ),
        (
            ROLLBACK_CSUM.to_owned(),
            manifest(OLDER_VERSION, Vec::new()),
        ),
    ];
    FakeState {
        running_manifest: Some(manifest(
            BOOTED_VERSION,
            vec![
                package("bash", "5.3.0", "2.fc45"),
                package("kernel", "6.17.1", "300.fc45"),
            ],
        )),
        manifests: manifests.into_iter().collect(),
        ..FakeState::default()
    }
}

pub struct Harness {
    pub fake: Arc<FakeRunner>,
    pub system: Arc<FakeSystem>,
    pub grub: SharedGrubWriter,
    pub store: StateDir,
    pub daemon: Daemon<Arc<FakeRunner>>,
    dir: TempDir,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(fake_state(), SharedGrubWriter::new())
    }

    pub fn with(state: FakeState, grub: SharedGrubWriter) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        Self::over(dir, state, grub)
    }

    fn over(dir: TempDir, state: FakeState, grub: SharedGrubWriter) -> Self {
        let store = StateDir::new(dir.path());
        let fake = Arc::new(FakeRunner::new());
        let system = FakeSystem::new(state);
        let engine = Engine::new(EngineParts {
            runner: Arc::clone(&fake),
            store: Arc::new(store.clone()),
            grub: grub.boxed(),
            grubenv: grub.source(),
            providers: system.providers(),
        });
        let daemon = Daemon::new(engine);
        daemon.spawn_worker();
        Self {
            fake,
            system,
            grub,
            store,
            daemon,
            dir,
        }
    }

    /// A new daemon over the same state files and grubenv; nothing kept in memory survives.
    pub fn reboot(self, now: u64) -> Self {
        self.fake.assert_finished();
        self.daemon.shutdown();
        let mut state = self.system.state();
        state.now = now;
        let grub = SharedGrubWriter::with_env(self.grub.env());
        Self::over(self.dir, state, grub)
    }

    pub fn push(&self, scripted: Scripted) -> &Self {
        self.fake.push(scripted);
        self
    }

    pub async fn run(&self, op: Op) -> Result<(), apex_dbus::ApexError> {
        self.daemon.submit(op).await
    }

    pub async fn refresh(&self, host: &Host) {
        self.push(status(host));
        self.run(Op::Refresh).await.expect("refresh");
    }

    pub fn last_check(&self, digest: &str, version: &str, age: u64) {
        self.store
            .write_last_check(&LastCheck {
                remote_size: None,
                at: NOW - age,
                remote_digest: Some(digest.into()),
                remote_version: Some(version.into()),
            })
            .expect("write last-check.json");
    }

    pub fn intent(&self) -> Option<Intent> {
        self.store.read_intent().expect("read intent.json")
    }

    pub fn properties(&self) -> apex_updated::properties::Properties {
        self.daemon.engine().properties()
    }
}
