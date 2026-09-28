//! Lenient serde model of `bootc status --format=json --format-version=1`.
//!
//! The authoritative schema is bootc's `docs/src/host-v1.schema.json`
//! (`apiVersion: org.containers.bootc/v1`, `kind: BootcHost`); the Rust source it is
//! generated from is `crates/lib/src/spec.rs`. bootc ships weekly in Fedora 45, so the
//! model follows three rules:
//!
//! - **No `deny_unknown_fields`.** New fields are ignored.
//! - **Enums have an `Unknown` variant** (`#[serde(other)]`), so a new value never fails
//!   the whole document.
//! - **Fields split by importance.** Fields the update engine decides on (digests,
//!   `downloadOnly`, `incompatible`, `pinned`, `rollbackQueued`, the ostree location)
//!   are typed strictly: if bootc changes their shape we want the qualify VM's contract
//!   test to fail loudly. Informational fields (`metadata`, `composefs`, `store`,
//!   `bootOrder`, `type`) fall back to their default when their shape changes.
//!   `usrOverlay` is special: an unparseable but present overlay is reported as an
//!   overlay of unknown kind, because treating it as absent would let automatic updates
//!   run on a modified `/usr`.

use std::collections::BTreeMap;

use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};

/// `apiVersion` emitted by `bootc status --format-version=1`.
pub const API_VERSION_V1: &str = "org.containers.bootc/v1";

/// Prefix shared by every bootc `apiVersion` (`v1`, and the older `v1alpha1`).
pub const API_GROUP_PREFIX: &str = "org.containers.bootc/";

/// `kind` of the status document.
pub const KIND_BOOTC_HOST: &str = "BootcHost";

/// Errors from [`Host::from_json`].
#[derive(Debug, thiserror::Error)]
pub enum StatusParseError {
    /// The document is not JSON, or a strictly typed field has the wrong shape.
    #[error("bootc status output does not match the v1 schema: {0}")]
    Json(#[from] serde_json::Error),
    /// `kind` is not `BootcHost`.
    #[error("unexpected kind {0:?} in bootc status output (expected \"BootcHost\")")]
    UnexpectedKind(String),
    /// `apiVersion` is not in the `org.containers.bootc/` group.
    #[error("unexpected apiVersion {0:?} in bootc status output")]
    UnexpectedApiVersion(String),
}

/// The whole status document (`kind: BootcHost`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    /// `org.containers.bootc/v1` for format version 1 (older fixtures say `v1alpha1`).
    #[serde(default)]
    pub api_version: String,
    /// Always `BootcHost`.
    #[serde(default)]
    pub kind: String,
    /// Kubernetes-style object metadata; informational only.
    #[serde(default, deserialize_with = "lenient")]
    pub metadata: ObjectMeta,
    /// What the host is configured to follow.
    #[serde(default, deserialize_with = "null_as_default")]
    pub spec: HostSpec,
    /// The deployments on disk and host-wide flags.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: HostStatus,
}

impl Host {
    /// Parses the stdout of `bootc status --format=json --format-version=1`.
    ///
    /// Only the document identity (`kind`, and the `apiVersion` group) and the strictly
    /// typed fields can make this fail; see the module documentation.
    pub fn from_json(json: impl AsRef<[u8]>) -> Result<Self, StatusParseError> {
        let host: Self = serde_json::from_slice(json.as_ref())?;
        if host.kind != KIND_BOOTC_HOST {
            return Err(StatusParseError::UnexpectedKind(host.kind));
        }
        if !host.api_version.starts_with(API_GROUP_PREFIX) {
            return Err(StatusParseError::UnexpectedApiVersion(host.api_version));
        }
        Ok(host)
    }
}

/// Object metadata (`metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMeta {
    /// Object name; bootc uses `host`.
    #[serde(default)]
    pub name: Option<String>,
    /// Object namespace; unused by bootc.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Labels. A `BTreeMap` keeps iteration (and therefore snapshots) deterministic.
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    /// Annotations, ordered for the same reason as `labels`.
    #[serde(default)]
    pub annotations: Option<BTreeMap<String, String>>,
}

/// Host specification (`spec`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSpec {
    /// The image the host follows; `None` on a host that is not bootc-managed.
    #[serde(default)]
    pub image: Option<ImageReference>,
    /// Which deployment boots next.
    #[serde(default, deserialize_with = "lenient")]
    pub boot_order: BootOrder,
}

/// `spec.bootOrder`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootOrder {
    /// The staged or booted deployment boots next.
    #[default]
    Default,
    /// The rollback deployment boots next.
    Rollback,
    /// A value this version of Apex does not know.
    #[serde(other)]
    Unknown,
}

/// A container image reference with transport and signature policy (`ImageReference`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageReference {
    /// The image, e.g. `ghcr.io/sekirokenjii/apex:45`.
    pub image: String,
    /// The transport, e.g. `registry`.
    pub transport: String,
    /// Signature verification mode; absent in older documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ImageSignature>,
}

/// Signature verification mode (`ImageSignature`).
///
/// bootc serialises this externally tagged: the strings `"containerPolicy"` and
/// `"insecure"`, or the object `{"ostreeRemote": "<remote>"}`. Deserialisation never
/// fails: any other shape becomes [`ImageSignature::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageSignature {
    /// Verified against the named ostree remote.
    OstreeRemote(String),
    /// Verified by `containers-policy.json`. This is what Apex uses.
    ContainerPolicy,
    /// No verification.
    Insecure,
    /// A form this version of Apex does not know.
    Unknown,
}

impl<'de> Deserialize<'de> for ImageSignature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value {
            serde_json::Value::String(s) => match s.as_str() {
                "containerPolicy" => Self::ContainerPolicy,
                "insecure" => Self::Insecure,
                _ => Self::Unknown,
            },
            serde_json::Value::Object(map) => match map.get("ostreeRemote") {
                Some(serde_json::Value::String(remote)) if map.len() == 1 => {
                    Self::OstreeRemote(remote.clone())
                }
                _ => Self::Unknown,
            },
            _ => Self::Unknown,
        })
    }
}

/// Host-wide status (`status`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    /// The deployment staged for the next boot.
    #[serde(default)]
    pub staged: Option<BootEntry>,
    /// The running deployment; `None` when the host is not bootc-managed.
    #[serde(default)]
    pub booted: Option<BootEntry>,
    /// The previous deployment.
    #[serde(default)]
    pub rollback: Option<BootEntry>,
    /// Further deployments, e.g. pinned ones. Omitted by bootc when empty.
    #[serde(default, deserialize_with = "null_as_default")]
    pub other_deployments: Vec<BootEntry>,
    /// The rollback deployment is queued for the next boot.
    #[serde(default)]
    pub rollback_queued: bool,
    /// Detected system type; `None` on a host that is not bootc-managed.
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    pub host_type: Option<HostType>,
    /// State of an overlay mounted on `/usr` (`bootc usr-overlay`).
    #[serde(default, deserialize_with = "usr_overlay")]
    pub usr_overlay: Option<FilesystemOverlay>,
    /// `/sysroot` is on read-only media (a live ISO); mutating commands are unavailable.
    #[serde(default, deserialize_with = "null_as_default")]
    pub read_only: bool,
}

/// `status.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostType {
    /// The host is deployed in a bootc-compatible way.
    BootcHost,
    /// A type this version of Apex does not know.
    #[serde(other)]
    Unknown,
}

/// One deployment (`BootEntry`).
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors bootc's BootEntry schema field for field"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntry {
    /// The image this deployment was created from; `None` for a plain ostree commit.
    #[serde(default)]
    pub image: Option<ImageStatus>,
    /// Metadata of the newest image found by the last fetch (`bootc upgrade --check`).
    #[serde(default)]
    pub cached_update: Option<ImageStatus>,
    /// The origin has changes bootc does not understand (e.g. rpm-ostree layering).
    #[serde(default)]
    pub incompatible: bool,
    /// The deployment is pinned and never garbage-collected.
    #[serde(default)]
    pub pinned: bool,
    /// A soft reboot into this deployment is possible.
    #[serde(default, deserialize_with = "null_as_default")]
    pub soft_reboot_capable: bool,
    /// The staged deployment is locked (`--download-only`) and will not be finalized.
    #[serde(default)]
    pub download_only: bool,
    /// Container storage backend.
    #[serde(default, deserialize_with = "lenient")]
    pub store: Option<Store>,
    /// ostree-specific state; present on the ostree backend Apex uses.
    #[serde(default)]
    pub ostree: Option<BootEntryOstree>,
    /// composefs-backend state; informational only (Apex uses the ostree backend).
    #[serde(default, deserialize_with = "lenient")]
    pub composefs: Option<BootEntryComposefs>,
}

/// Image metadata of a deployment (`ImageStatus`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageStatus {
    /// The image reference the deployment was pulled from.
    pub image: ImageReference,
    /// `org.opencontainers.image.version`, if the image has one.
    #[serde(default)]
    pub version: Option<String>,
    /// Build timestamp (RFC 3339), if the image has one.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Manifest digest, `sha256:…`.
    pub image_digest: String,
    /// Image architecture, e.g. `amd64`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub architecture: String,
}

/// ostree state of a deployment (`BootEntryOstree`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntryOstree {
    /// Name of the storage for `/etc` and `/var` (usually `default`).
    pub stateroot: String,
    /// The ostree commit checksum.
    pub checksum: String,
    /// The deployment serial.
    pub deploy_serial: u32,
}

/// composefs state of a deployment (`BootEntryComposefs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntryComposefs {
    /// The erofs verity digest.
    #[serde(default)]
    pub verity: String,
    /// Type 1 (BLS) or Type 2 (UKI) boot entry.
    #[serde(default, deserialize_with = "lenient")]
    pub boot_type: BootType,
    /// Bootloader in use.
    #[serde(default, deserialize_with = "lenient")]
    pub bootloader: Bootloader,
    /// sha256 of vmlinuz + initrd, for Type 1 entries.
    #[serde(default)]
    pub boot_digest: Option<String>,
    /// fs-verity validation is optional.
    #[serde(default)]
    pub missing_verity_allowed: bool,
}

/// `BootEntryComposefs.bootType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootType {
    /// Boot Loader Specification Type 1 entry.
    Bls,
    /// Unified kernel image (Type 2).
    Uki,
    /// A value this version of Apex does not know.
    #[default]
    #[serde(other)]
    Unknown,
}

/// `BootEntryComposefs.bootloader`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bootloader {
    /// GRUB.
    Grub,
    /// GRUB for confidential computing.
    GrubCc,
    /// systemd-boot.
    Systemd,
    /// No bootloader managed by bootc.
    None,
    /// A value this version of Apex does not know.
    #[default]
    #[serde(other)]
    Unknown,
}

/// `BootEntry.store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Store {
    /// The ostree-container storage backend.
    OstreeContainer,
    /// A value this version of Apex does not know.
    #[serde(other)]
    Unknown,
}

/// An overlay mounted on `/usr` (`FilesystemOverlay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemOverlay {
    /// Read-only or read/write.
    pub access_mode: FilesystemOverlayAccessMode,
    /// Transient or persistent.
    pub persistence: FilesystemOverlayPersistence,
}

impl FilesystemOverlay {
    /// An overlay that exists but whose description could not be parsed.
    pub const UNKNOWN: Self = Self {
        access_mode: FilesystemOverlayAccessMode::Unknown,
        persistence: FilesystemOverlayPersistence::Unknown,
    };
}

/// `FilesystemOverlay.accessMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilesystemOverlayAccessMode {
    /// Mounted read-only.
    ReadOnly,
    /// Mounted read/write.
    ReadWrite,
    /// A value this version of Apex does not know.
    #[serde(other)]
    Unknown,
}

/// `FilesystemOverlay.persistence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilesystemOverlayPersistence {
    /// Lost on reboot.
    Transient,
    /// Survives reboots.
    Persistent,
    /// A value this version of Apex does not know.
    #[serde(other)]
    Unknown,
}

/// Deserialises `T`, treating `null` like a missing field.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Deserialises an informational field, falling back to `T::default()` when its shape
/// is not what this version expects. Only malformed JSON is an error.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// `usrOverlay`: `null`/absent is "no overlay"; anything else is an overlay, of unknown
/// kind if it cannot be parsed. Failing safe matters here: the update gates stop
/// automatic operations while an overlay exists.
fn usr_overlay<'de, D>(deserializer: D) -> Result<Option<FilesystemOverlay>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(
        FilesystemOverlay::deserialize(value).unwrap_or(FilesystemOverlay::UNKNOWN),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(status: &str) -> Host {
        let doc = format!(
            r#"{{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","status":{status}}}"#
        );
        Host::from_json(doc).unwrap()
    }

    #[test]
    fn minimal_document_parses_with_defaults() {
        let h = Host::from_json(r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost"}"#)
            .unwrap();
        assert_eq!(h.status, HostStatus::default());
        assert_eq!(h.spec.boot_order, BootOrder::Default);
        assert!(h.metadata.name.is_none());
    }

    #[test]
    fn wrong_kind_and_api_group_are_rejected() {
        let err = Host::from_json(r#"{"apiVersion":"org.containers.bootc/v1","kind":"Pod"}"#)
            .unwrap_err();
        assert!(matches!(err, StatusParseError::UnexpectedKind(k) if k == "Pod"));
        let err = Host::from_json(r#"{"apiVersion":"v1","kind":"BootcHost"}"#).unwrap_err();
        assert!(matches!(err, StatusParseError::UnexpectedApiVersion(_)));
        assert!(matches!(
            Host::from_json("not json"),
            Err(StatusParseError::Json(_))
        ));
    }

    #[test]
    fn unknown_fields_and_enum_values_are_tolerated() {
        let h = Host::from_json(
            r#"{"apiVersion":"org.containers.bootc/v2beta","kind":"BootcHost","newTopLevel":1,
               "metadata":{"name":7},
               "spec":{"image":{"image":"x/y:1","transport":"registry","signature":{"sigstore":{}}},
                       "bootOrder":"sideways","future":true},
               "status":{"type":"composefsHost","booted":{"store":"somethingElse",
                 "composefs":{"verity":"ab","bootType":"Uki","bootloader":"refind","missingVerityAllowed":false},
                 "image":{"image":{"image":"x/y:1","transport":"registry"},"imageDigest":"sha256:00",
                          "architecture":"amd64","newField":[1,2]},
                 "incompatible":false,"pinned":false,"brandNew":{}}}}"#,
        )
        .unwrap();
        assert!(
            h.metadata.name.is_none(),
            "wrongly typed metadata falls back to default"
        );
        assert_eq!(h.spec.boot_order, BootOrder::Unknown);
        let image = h.spec.image.as_ref().unwrap();
        assert_eq!(image.signature, Some(ImageSignature::Unknown));
        assert_eq!(h.status.host_type, Some(HostType::Unknown));
        let booted = h.status.booted.as_ref().unwrap();
        assert_eq!(booted.store, Some(Store::Unknown));
        let cfs = booted.composefs.as_ref().unwrap();
        assert_eq!(cfs.boot_type, BootType::Uki);
        assert_eq!(cfs.bootloader, Bootloader::Unknown);
    }

    #[test]
    fn signature_forms() {
        let parse = |s: &str| -> ImageSignature { serde_json::from_str(s).unwrap() };
        assert_eq!(
            parse(r#""containerPolicy""#),
            ImageSignature::ContainerPolicy
        );
        assert_eq!(parse(r#""insecure""#), ImageSignature::Insecure);
        assert_eq!(
            parse(r#"{"ostreeRemote":"fedora"}"#),
            ImageSignature::OstreeRemote("fedora".into())
        );
        assert_eq!(parse(r#"{"ostreeRemote":1}"#), ImageSignature::Unknown);
        assert_eq!(parse("42"), ImageSignature::Unknown);
    }

    #[test]
    fn usr_overlay_fails_safe() {
        assert_eq!(host(r#"{"usrOverlay":null}"#).status.usr_overlay, None);
        assert_eq!(host("{}").status.usr_overlay, None);
        assert_eq!(
            host(r#"{"usrOverlay":{"accessMode":"readWrite","persistence":"transient"}}"#)
                .status
                .usr_overlay,
            Some(FilesystemOverlay {
                access_mode: FilesystemOverlayAccessMode::ReadWrite,
                persistence: FilesystemOverlayPersistence::Transient,
            })
        );
        assert_eq!(
            host(r#"{"usrOverlay":"yes"}"#).status.usr_overlay,
            Some(FilesystemOverlay::UNKNOWN)
        );
    }

    #[test]
    fn decision_fields_are_strict() {
        // A wrongly typed digest must fail loudly rather than silently default.
        let doc = r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","status":{"booted":
            {"image":{"image":{"image":"x","transport":"registry"},"imageDigest":5,"architecture":"amd64"},
             "incompatible":false,"pinned":false}}}"#;
        assert!(Host::from_json(doc).is_err());
        let doc = r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","status":{"rollbackQueued":"yes"}}"#;
        assert!(Host::from_json(doc).is_err());
    }

    #[test]
    fn nulls_become_defaults() {
        let h = Host::from_json(
            r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","metadata":null,"spec":null,
                "status":{"otherDeployments":null,"readOnly":null,"type":null}}"#,
        )
        .unwrap();
        assert!(h.status.other_deployments.is_empty());
        assert!(!h.status.read_only);
        assert_eq!(h.status.host_type, None);
    }
}
