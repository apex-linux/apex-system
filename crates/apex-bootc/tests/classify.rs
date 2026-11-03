use std::fmt::Write as _;

use apex_bootc::{CommandKind, ErrorKind, ExitState, classify_failure};

const ONE: ExitState = ExitState::Code(1);

const CASES: &[(ExitState, &str, ErrorKind)] = &[
    (
        ONE,
        "error: Upgrading: Initializing storage: System not booted via bootc",
        ErrorKind::NotBootcHost,
    ),
    (
        ONE,
        "error: Rolling back: Not currently booted into an OSTree system",
        ErrorKind::NotBootcHost,
    ),
    (
        ONE,
        "Waiting for sysroot lock...\nerror: Upgrading: interrupted",
        ErrorKind::Busy,
    ),
    (
        ONE,
        "error: Transaction in progress: deploy\n You can cancel the current transaction with `rpm-ostree cancel`",
        ErrorKind::Busy,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: Importing: Writing content object: No space left on device (os error 28)",
        ErrorKind::NoSpace,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: Insufficient free space for ghcr.io/apex-linux/apex:45 (available: 1.2 GB required: 3.4 GB)",
        ErrorKind::NoSpace,
    ),
    (
        ONE,
        "error: Upgrading: Staging: min-free-space-percent '3%' would be exceeded, at least 13.2 MB requested",
        ErrorKind::NoSpace,
    ),
    (
        ONE,
        "error: Upgrading: Staging: min-free-space-size 500MB would be exceeded",
        ErrorKind::NoSpace,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: Creating importer: failed to invoke method OpenImage: A signature was required, but no signature exists",
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        "error: Switching (ostree): Pulling: failed to invoke method OpenImage: Source image rejected: Running image docker://ghcr.io/evil/apex:45 is rejected by policy.",
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: failed to invoke method OpenImage: None of the signatures were accepted, reasons: cryptographic signature verification failed: invalid signature when validating ASN.1 encoded signature",
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        r#"error: Upgrading: Pulling: failed to invoke method OpenImage: Signature for identity "ghcr.io/apex-linux/apex:45" is not accepted"#,
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        "error: Switching (ostree): failed to invoke method OpenImage: No signature verification policy found for image docker://quay.io/x/y:1",
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: containers-policy.json specifies a default of `insecureAcceptAnything`; refusing usage",
        ErrorKind::SignatureRejected,
    ),
    (
        ONE,
        r#"error: Upgrading: Pulling: failed to invoke method OpenImage: pinging container registry ghcr.io: Get "https://ghcr.io/v2/": dial tcp: lookup ghcr.io on 127.0.0.53:53: server misbehaving"#,
        ErrorKind::Network,
    ),
    (
        ONE,
        r#"error: Upgrading: Pulling: reading blob sha256:ab: Get "https://pkg-containers.githubusercontent.com/…": read tcp 10.0.0.2:44310->185.199.108.154:443: read: connection reset by peer"#,
        ErrorKind::Network,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: failed to invoke method OpenImage: reading manifest 45 in ghcr.io/apex-linux/apex: too many requests to registry",
        ErrorKind::Network,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: fetching blob: received unexpected HTTP status: 503 Service Unavailable",
        ErrorKind::Network,
    ),
    (
        ONE,
        r#"error: Upgrading: Pulling: Get "https://ghcr.io/v2/": net/http: TLS handshake timeout"#,
        ErrorKind::Network,
    ),
    (
        ONE,
        "error: Upgrading: Pulling: dial tcp 140.82.114.33:443: connect: network is unreachable",
        ErrorKind::Network,
    ),
    // Only with clap's exit status 2 is a usage error unsupported.
    (
        ExitState::Code(2),
        "error: unexpected argument '--progress-fd' found\n\n  tip: to pass '--progress-fd' as a value, use '-- --progress-fd'\n\nUsage: bootc upgrade [OPTIONS]\n\nFor more information, try '--help'.\n",
        ErrorKind::Unsupported,
    ),
    (
        ExitState::Code(2),
        "error: unrecognized subcommand 'usr-overlay'\n\nUsage: bootc <COMMAND>\n",
        ErrorKind::Unsupported,
    ),
    (
        ONE,
        "error: unexpected argument '--progress-fd' found",
        ErrorKind::Other,
    ),
    (
        ONE,
        "error: Upgrading: Deployment contains local rpm-ostree modifications; cannot upgrade via bootc. You can run `rpm-ostree reset` to undo the modifications.",
        ErrorKind::Other,
    ),
    (
        ONE,
        "error: Upgrading: No staged deployment found",
        ErrorKind::Other,
    ),
    (
        ONE,
        "error: Upgrading: Cannot perform this operation: /sysroot is on a read-only block device (e.g. a live ISO)",
        ErrorKind::Other,
    ),
    (ExitState::Signal(9), "", ErrorKind::Other),
    (ONE, "", ErrorKind::Other),
    (
        ONE,
        "\u{1b}[31merror: \u{1b}[0mUpgrading: Pulling: NO SPACE LEFT ON DEVICE",
        ErrorKind::NoSpace,
    ),
    // Precedence: disk space and signature win over the registry text around them.
    (
        ONE,
        "error: Upgrading: pinging container registry ghcr.io: write /var/tmp/x: no space left on device",
        ErrorKind::NoSpace,
    ),
    (
        ONE,
        "error: Upgrading: reading manifest in ghcr.io/x: A signature was required, but no signature exists",
        ErrorKind::SignatureRejected,
    ),
];

#[test]
fn classifies_sample_stderr() {
    for (exit, stderr, expected) in CASES {
        let err = classify_failure(CommandKind::UpgradeDownloadOnly, *exit, stderr.as_bytes());
        assert_eq!(err.kind(), *expected, "{stderr:?} ({exit}) -> {err}");
    }
}

#[test]
fn classification_snapshot() {
    let mut out = String::new();
    for (exit, stderr, _) in CASES {
        let err = classify_failure(CommandKind::UpgradeDownloadOnly, *exit, stderr.as_bytes());
        writeln!(out, "{:?}: {err}", err.kind()).unwrap();
    }
    insta::assert_snapshot!(out);
}

#[test]
fn invalid_utf8_and_huge_stderr_are_handled() {
    let mut bytes = b"error: Upgrading: \xff\xfe No space left on device".to_vec();
    let err = classify_failure(CommandKind::Status, ONE, &bytes);
    assert_eq!(err.kind(), ErrorKind::NoSpace);
    bytes = vec![b'x'; 2 << 20];
    let err = classify_failure(CommandKind::Status, ONE, &bytes);
    assert_eq!(err.kind(), ErrorKind::Other);
    assert!(err.to_string().len() < 1024);
}

#[test]
fn transient_errors() {
    let network = classify_failure(
        CommandKind::UpgradeCheck,
        ONE,
        b"error: dial tcp: i/o timeout",
    );
    assert!(network.is_transient());
    let signature = classify_failure(
        CommandKind::UpgradeCheck,
        ONE,
        b"error: A signature was required, but no signature exists",
    );
    assert!(!signature.is_transient());
}

/// The next Fedora major's tag before it is published (`bootc switch` to `…/apex:46`).
#[test]
fn a_missing_tag_is_image_not_found_and_final() {
    for stderr in [
        "error: Switching (ostree): Pulling: Creating importer: failed to invoke method OpenImage: \
         reading manifest 46 in ghcr.io/apex-linux/apex: manifest unknown",
        "error: Switching (ostree): Pulling: failed to invoke method OpenImage: reading manifest \
         46 in ghcr.io/apex-linux/apex: StatusCode: 404, 404 page not found",
    ] {
        let err = classify_failure(CommandKind::SwitchDownloadOnly, ONE, stderr.as_bytes());
        assert_eq!(err.kind(), ErrorKind::ImageNotFound, "{stderr:?} -> {err}");
        assert!(!err.is_transient());
        assert!(err.to_string().contains("ghcr.io/apex-linux/apex"), "{err}");
    }
    let refused = classify_failure(
        CommandKind::SwitchDownloadOnly,
        ONE,
        b"error: reading manifest 46 in ghcr.io/x: A signature was required, but no signature exists",
    );
    assert_eq!(refused.kind(), ErrorKind::SignatureRejected);
    let offline = classify_failure(
        CommandKind::SwitchDownloadOnly,
        ONE,
        b"error: pinging container registry ghcr.io: dial tcp: no such host",
    );
    assert_eq!(offline.kind(), ErrorKind::Network);
}
