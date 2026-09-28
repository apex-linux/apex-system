#![allow(
    dead_code,
    reason = "each test binary uses a different part of this module"
)]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use apex_dbus::{
    ApexError, BootVerdict, Change, ChangeKind, Channel, Deployment, ErrorKind, INTERFACE, State,
};
use zbus::{
    message::{Flags, Header},
    object_server::SignalEmitter,
};

pub(crate) const STAGED_DIGEST: &str =
    "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) member: String,
    /// The caller set `ALLOW_INTERACTIVE_AUTHORIZATION`.
    pub(crate) interactive: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Log {
    calls: Arc<Mutex<Vec<Call>>>,
    properties: Arc<Mutex<Vec<String>>>,
}

impl Log {
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn properties(&self) -> Vec<String> {
        self.properties
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn call(&self, header: &Header<'_>) {
        let member = header.member().map(ToString::to_string).unwrap_or_default();
        let interactive = header
            .primary()
            .flags()
            .contains(Flags::AllowInteractiveAuth);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call {
                member,
                interactive,
            });
    }

    fn property(&self, name: &str) {
        self.properties
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name.to_owned());
    }
}

pub(crate) fn booted() -> Deployment {
    Deployment {
        image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
        digest: Some(
            "sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_owned(),
        ),
        version: Some("45.20260920.3".to_owned()),
        timestamp: Some(1_790_000_000),
        pinned: Some(false),
        download_only: None,
        degraded: Some(vec!["audio".to_owned()]),
    }
}

pub(crate) fn staged() -> Deployment {
    Deployment {
        image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
        digest: Some(STAGED_DIGEST.to_owned()),
        version: Some("45.20260924.1".to_owned()),
        timestamp: Some(1_790_300_000),
        pinned: Some(false),
        download_only: Some(true),
        degraded: Some(Vec::new()),
    }
}

pub(crate) fn changes() -> Vec<Change> {
    let change = |name: &str, arch: &str, kind: ChangeKind, from: &str, to: &str| {
        (
            name.to_owned(),
            arch.to_owned(),
            kind.as_str().to_owned(),
            from.to_owned(),
            to.to_owned(),
        )
    };
    vec![
        change("apex", "noarch", ChangeKind::Added, "", "0.1.0-1.fc45"),
        change(
            "glibc",
            "i686",
            ChangeKind::Upgraded,
            "2.42-4.fc45",
            "2.42-5.fc45",
        ),
        change(
            "glibc",
            "x86_64",
            ChangeKind::Upgraded,
            "2.42-4.fc45",
            "2.42-5.fc45",
        ),
        change("oldpkg", "x86_64", ChangeKind::Removed, "1.0-1.fc45", ""),
    ]
}

fn scripted_error(what: &str) -> ApexError {
    match what {
        // A daemon that forgets `into_contract` leaks a non-contract error name.
        "unconverted" => ApexError::ZBus(zbus::Error::Failure("unconverted".to_owned())),
        "zbus" => ApexError::ZBus(zbus::Error::Failure("socket closed".to_owned())).into_contract(),
        kind => ErrorKind::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == kind)
            .map_or_else(
                || ApexError::InvalidArgument(format!("unknown scripted error {kind}")),
                |kind| ApexError::new(kind, format!("{} from the mock", kind.as_str())),
            ),
    }
}

#[derive(Debug)]
pub(crate) struct MockDaemon {
    log: Log,
    state: State,
}

impl MockDaemon {
    pub(crate) fn new(log: Log) -> Self {
        Self {
            log,
            state: State::Staged,
        }
    }
}

// zbus derives the D-Bus member and argument names from these Rust names.
#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "zbus hands the header over by value; handlers keep the contract's Result shape"
)]
#[zbus::interface(name = "io.github.sekirokenjii.Apex.Update1")]
impl MockDaemon {
    fn ping(&self, #[zbus(header)] header: Header<'_>) {
        self.log.call(&header);
    }

    async fn check(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        self.state = State::Available;
        // Never let a zbus error leave with a non-contract name.
        self.state_changed(&emitter)
            .await
            .map_err(|error| ApexError::from(error).into_contract())
    }

    fn download(
        &self,
        override_soft_gates: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        if override_soft_gates {
            Ok(())
        } else {
            Err(ApexError::PolicyDenied("On battery below 50 %".to_owned()))
        }
    }

    fn apply(
        &self,
        expected_digest: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        if let Some(what) = expected_digest.strip_prefix("error:") {
            return Err(scripted_error(what));
        }
        if expected_digest == STAGED_DIGEST {
            Ok(())
        } else {
            Err(ApexError::DigestMismatch(format!(
                "staged {STAGED_DIGEST}, expected {expected_digest}"
            )))
        }
    }

    fn discard_staged(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        self.log.call(&header);
        Ok(())
    }

    fn rollback(
        &self,
        mark_bad: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        if mark_bad {
            Ok(())
        } else {
            Err(ApexError::NothingToDo("no rollback deployment".to_owned()))
        }
    }

    #[zbus(out_args("changes"))]
    fn get_changes(&self, #[zbus(header)] header: Header<'_>) -> Result<Vec<Change>, ApexError> {
        self.log.call(&header);
        Ok(changes())
    }

    /// Takes a raw string: zbus would reject a typed argument with a generic
    /// `org.freedesktop.DBus.Error.InvalidArgs` instead of the contract's `InvalidArgument`.
    fn switch_channel(
        &self,
        channel: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        let channel: Channel = channel
            .parse()
            .map_err(|error: apex_dbus::UnknownValueError| {
                ApexError::InvalidArgument(error.to_string())
            })?;
        match channel {
            Channel::Testing => Ok(()),
            Channel::Stable => Err(ApexError::NothingToDo("already on stable".to_owned())),
        }
    }

    fn acknowledge_health_failure(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        Ok(())
    }

    fn automatic(&self, #[zbus(header)] header: Header<'_>) -> Result<(), ApexError> {
        self.log.call(&header);
        Err(ApexError::NotAuthorized("uid 0 only".to_owned()))
    }

    fn report_boot(
        &self,
        verdict: &str,
        digest: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), ApexError> {
        self.log.call(&header);
        let verdict: BootVerdict =
            verdict
                .parse()
                .map_err(|error: apex_dbus::UnknownValueError| {
                    ApexError::InvalidArgument(error.to_string())
                })?;
        if verdict == BootVerdict::Green && digest.is_empty() {
            return Err(ApexError::InvalidArgument(
                "green needs the booted digest".to_owned(),
            ));
        }
        Ok(())
    }

    #[zbus(property)]
    fn state(&self) -> State {
        self.log.property("State");
        self.state
    }

    #[zbus(property)]
    fn progress(&self) -> f64 {
        self.log.property("Progress");
        -1.0
    }

    #[zbus(property)]
    fn booted(&self) -> Deployment {
        self.log.property("Booted");
        booted()
    }

    #[zbus(property)]
    fn staged(&self) -> Deployment {
        self.log.property("Staged");
        staged()
    }

    #[zbus(property, name = "Rollback")]
    fn rollback_deployment(&self) -> Deployment {
        self.log.property("Rollback");
        Deployment::default()
    }

    #[zbus(property)]
    fn available(&self) -> Deployment {
        self.log.property("Available");
        Deployment::default()
    }

    #[zbus(property)]
    fn channel(&self) -> Channel {
        self.log.property("Channel");
        Channel::Stable
    }

    #[zbus(property)]
    fn last_check(&self) -> u64 {
        self.log.property("LastCheck");
        1_790_400_000
    }

    #[zbus(property)]
    fn deferred_reason(&self) -> String {
        self.log.property("DeferredReason");
        "Metered network".to_owned()
    }

    #[zbus(property)]
    fn last_error(&self) -> String {
        self.log.property("LastError");
        String::new()
    }

    #[zbus(property)]
    fn stale(&self) -> bool {
        self.log.property("Stale");
        true
    }

    #[zbus(property)]
    fn health_failure(&self) -> String {
        self.log.property("HealthFailure");
        String::new()
    }
}

/// One entry per member, e.g. `"method Download" => "in override_soft_gates b"`.
pub(crate) fn interface_description(xml: &str) -> BTreeMap<String, String> {
    let node = zbus_xml::Node::try_from(xml).expect("introspection XML parses");
    let interface = node
        .interfaces()
        .iter()
        .find(|interface| interface.name().as_str() == INTERFACE)
        .unwrap_or_else(|| panic!("no interface {INTERFACE}"));
    let emits = |annotations: &[zbus_xml::Annotation], default: &str| {
        annotations
            .iter()
            .find(|a| a.name() == "org.freedesktop.DBus.Property.EmitsChangedSignal")
            .map_or(default.to_owned(), |a| a.value().to_owned())
    };
    // Per the D-Bus specification the default is "true", overridable per interface.
    let interface_emits = emits(interface.annotations(), "true");
    let args = |args: &[zbus_xml::Arg], default: zbus_xml::ArgDirection| {
        args.iter()
            .map(|arg| {
                let direction = match arg.direction().unwrap_or(default) {
                    zbus_xml::ArgDirection::In => "in",
                    zbus_xml::ArgDirection::Out => "out",
                };
                format!(
                    "{direction} {} {}",
                    arg.name().unwrap_or("<unnamed>"),
                    arg.ty().to_string()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut description = BTreeMap::new();
    for method in interface.methods() {
        description.insert(
            format!("method {}", method.name()),
            args(method.args(), zbus_xml::ArgDirection::In),
        );
    }
    for signal in interface.signals() {
        description.insert(
            format!("signal {}", signal.name()),
            args(signal.args(), zbus_xml::ArgDirection::Out),
        );
    }
    for property in interface.properties() {
        let access = match property.access() {
            zbus_xml::PropertyAccess::Read => "read",
            zbus_xml::PropertyAccess::Write => "write",
            zbus_xml::PropertyAccess::ReadWrite => "readwrite",
        };
        description.insert(
            format!("property {}", property.name()),
            format!(
                "{} {access} emits-changed={}",
                property.ty().to_string(),
                emits(property.annotations(), &interface_emits)
            ),
        );
    }
    description
}

pub(crate) fn diff(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut differences = Vec::new();
    for (member, want) in expected {
        match actual.get(member) {
            None => differences.push(format!("{member}: missing (contract: {want})")),
            Some(have) if have != want => {
                differences.push(format!("{member}: contract {want:?}, found {have:?}"));
            }
            Some(_) => {}
        }
    }
    for (member, have) in actual {
        if !expected.contains_key(member) {
            differences.push(format!("{member}: not in the contract ({have})"));
        }
    }
    differences
}
