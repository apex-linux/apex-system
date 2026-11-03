//! `apexctl major-upgrade`: `MajorUpgrade` for the next Fedora major. The daemon refuses a
//! target other than the running major plus 1, so the boot cannot change under the plan.

use std::fmt::Write as _;

use apex_dbus::ErrorKind;

use crate::service::{ClientError, ServiceStatus, UpdateService};

/// From the booted version, `FEDORA.YYYYMMDD.RUN`.
pub fn booted_major(status: &ServiceStatus) -> Result<u32, ClientError> {
    status
        .booted
        .version
        .as_deref()
        .and_then(|version| version.split('.').next()?.parse().ok())
        .ok_or_else(|| {
            ClientError::NotReady(
                "The running version is not known, so the next release cannot be named.".to_owned(),
            )
        })
}

pub fn plan(from: u32) -> String {
    let to = from + 1;
    format!(
        "Moving Apex {from} to Apex {to}: download Apex {to} locked, keep Apex {from} pinned in \
         the boot menu, save /var as a read-only snapshot, then install Apex {to} for the next \
         restart.\n"
    )
}

pub async fn major_upgrade<S: UpdateService>(
    service: &S,
    from: u32,
    force: bool,
) -> Result<String, ClientError> {
    let to = from + 1;
    service.major_upgrade(to, force).await?;
    let status = service.status().await?;
    let mut out = format!(
        "Apex {to} is installed for the next restart. Restart to finish; apexctl never restarts \
         the computer.\n"
    );
    let snapshot = status
        .snapshots
        .iter()
        .rfind(|(_, major, _)| *major == from)
        .map(|(path, _, _)| path);
    match snapshot {
        Some(path) => {
            let _ = writeln!(
                out,
                "Apex {from} stays pinned in the boot menu, and /var was saved as {path}."
            );
        }
        None => {
            let _ = writeln!(out, "Apex {from} stays pinned in the boot menu.");
        }
    }
    Ok(out)
}

/// The refusals say what to do themselves; `download --force` is not this command's hint.
pub fn hint(error: &ClientError) -> Option<&'static str> {
    match error {
        ClientError::Daemon(daemon) if daemon.kind() == Some(ErrorKind::PolicyDenied) => None,
        _ => error.hint(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::future::{Future, ready};

    use apex_dbus::{ApexError, Change, Deployment};

    use super::*;
    use crate::exit::Exit;
    use crate::status::tests::staged_status;

    struct Fake {
        status: ServiceStatus,
        refuse: Option<ErrorKind>,
        calls: RefCell<Vec<(u32, bool)>>,
    }

    impl UpdateService for Fake {
        fn status(&self) -> impl Future<Output = Result<ServiceStatus, ClientError>> {
            ready(Ok(self.status.clone()))
        }
        fn check(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn download(&self, _: bool) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn apply(&self, _: &str) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn rollback(&self, _: bool) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn changes(&self) -> impl Future<Output = Result<Vec<Change>, ClientError>> {
            ready(Ok(Vec::new()))
        }
        fn changes_for(&self, _: &str) -> impl Future<Output = Result<Vec<Change>, ClientError>> {
            ready(Ok(Vec::new()))
        }
        fn acknowledge_health_failure(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn automatic(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn set_automatic_downloads(
            &self,
            _: bool,
        ) -> impl Future<Output = Result<(), ClientError>> {
            ready(Ok(()))
        }
        fn major_upgrade(
            &self,
            target_major: u32,
            force: bool,
        ) -> impl Future<Output = Result<(), ClientError>> {
            self.calls.borrow_mut().push((target_major, force));
            ready(match self.refuse {
                Some(kind) => Err(ClientError::Daemon(ApexError::new(kind, "refused"))),
                None => Ok(()),
            })
        }
    }

    fn on_45() -> ServiceStatus {
        ServiceStatus {
            booted: Deployment {
                version: Some("45.20270410.3".to_owned()),
                ..Deployment::default()
            },
            snapshots: vec![(
                "/var/lib/apex/snapshots/var-fedora45-1790300000".to_owned(),
                45,
                1_790_300_000,
            )],
            ..staged_status()
        }
    }

    fn fake(refuse: Option<ErrorKind>) -> Fake {
        Fake {
            status: on_45(),
            refuse,
            calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn the_target_is_the_running_major_plus_one() {
        assert_eq!(booted_major(&on_45()).unwrap(), 45);
        let unknown = ServiceStatus {
            booted: Deployment::default(),
            ..staged_status()
        };
        let error = booted_major(&unknown).unwrap_err();
        assert_eq!(error.exit(), Exit::Error);
        assert!(plan(45).starts_with("Moving Apex 45 to Apex 46: download Apex 46 locked"));
    }

    #[tokio::test]
    async fn success_names_the_kept_version_and_the_snapshot() {
        let fake = fake(None);
        let text = major_upgrade(&fake, 45, true).await.unwrap();
        assert_eq!(*fake.calls.borrow(), [(46, true)]);
        assert_eq!(
            text,
            "Apex 46 is installed for the next restart. Restart to finish; apexctl never \
             restarts the computer.\nApex 45 stays pinned in the boot menu, and /var was saved \
             as /var/lib/apex/snapshots/var-fedora45-1790300000.\n"
        );
    }

    #[tokio::test]
    async fn refusals_keep_the_exit_contract() {
        for (kind, exit) in [
            (ErrorKind::NothingToDo, Exit::NothingToDo),
            (ErrorKind::PolicyDenied, Exit::PolicyDenied),
            (ErrorKind::NotAuthorized, Exit::Error),
            (ErrorKind::Bootc, Exit::Error),
        ] {
            let fake = fake(Some(kind));
            let error = major_upgrade(&fake, 45, false).await.unwrap_err();
            assert_eq!(error.exit(), exit, "{kind:?}");
            if kind == ErrorKind::PolicyDenied {
                assert_eq!(hint(&error), None);
            }
        }
    }
}
