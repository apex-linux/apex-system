//! Lenient model of bootc's `docs/src/host-v1.schema.json`. Fields the update engine decides on
//! are strict, so a schema change fails loudly; informational ones fall back to their default.

use std::collections::BTreeMap;

use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};

pub const API_VERSION_V1: &str = "org.containers.bootc/v1";

/// Older documents say `v1alpha1`.
pub const API_GROUP_PREFIX: &str = "org.containers.bootc/";

pub const KIND_BOOTC_HOST: &str = "BootcHost";

#[derive(Debug, thiserror::Error)]
pub enum StatusParseError {
    #[error("bootc status output does not match the v1 schema: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected kind {0:?} in bootc status output (expected \"BootcHost\")")]
    UnexpectedKind(String),
    #[error("unexpected apiVersion {0:?} in bootc status output")]
    UnexpectedApiVersion(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub metadata: ObjectMeta,
    #[serde(default, deserialize_with = "null_as_default")]
    pub spec: HostSpec,
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: HostStatus,
}

impl Host {
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

/// `BTreeMap`s keep iteration, and so snapshots, deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSpec {
    #[serde(default)]
    pub image: Option<ImageReference>,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub boot_order: BootOrder,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootOrder {
    #[default]
    Default,
    Rollback,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageReference {
    pub image: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ImageSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageSignature {
    OstreeRemote(String),
    ContainerPolicy,
    Insecure,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    #[serde(default)]
    pub staged: Option<BootEntry>,
    #[serde(default)]
    pub booted: Option<BootEntry>,
    #[serde(default)]
    pub rollback: Option<BootEntry>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub other_deployments: Vec<BootEntry>,
    #[serde(default)]
    pub rollback_queued: bool,
    #[serde(
        rename = "type",
        default,
        deserialize_with = "default_on_unexpected_shape"
    )]
    pub host_type: Option<HostType>,
    #[serde(default, deserialize_with = "usr_overlay")]
    pub usr_overlay: Option<FilesystemOverlay>,
    /// `/sysroot` is on read-only media (a live ISO): mutating commands are unavailable.
    #[serde(default, deserialize_with = "null_as_default")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostType {
    BootcHost,
    #[serde(other)]
    Unknown,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors bootc's BootEntry schema field for field"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntry {
    #[serde(default)]
    pub image: Option<ImageStatus>,
    #[serde(default)]
    pub cached_update: Option<ImageStatus>,
    #[serde(default)]
    pub incompatible: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub soft_reboot_capable: bool,
    /// The stage is locked and will not be finalized at shutdown.
    #[serde(default)]
    pub download_only: bool,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub store: Option<Store>,
    #[serde(default)]
    pub ostree: Option<BootEntryOstree>,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub composefs: Option<BootEntryComposefs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageStatus {
    pub image: ImageReference,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    pub image_digest: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntryOstree {
    pub stateroot: String,
    pub checksum: String,
    pub deploy_serial: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootEntryComposefs {
    #[serde(default)]
    pub verity: String,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub boot_type: BootType,
    #[serde(default, deserialize_with = "default_on_unexpected_shape")]
    pub bootloader: Bootloader,
    #[serde(default)]
    pub boot_digest: Option<String>,
    #[serde(default)]
    pub missing_verity_allowed: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootType {
    Bls,
    Uki,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bootloader {
    Grub,
    GrubCc,
    Systemd,
    None,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Store {
    OstreeContainer,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemOverlay {
    pub access_mode: FilesystemOverlayAccessMode,
    pub persistence: FilesystemOverlayPersistence,
}

impl FilesystemOverlay {
    pub const UNKNOWN: Self = Self {
        access_mode: FilesystemOverlayAccessMode::Unknown,
        persistence: FilesystemOverlayPersistence::Unknown,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilesystemOverlayAccessMode {
    ReadOnly,
    ReadWrite,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilesystemOverlayPersistence {
    Transient,
    Persistent,
    #[serde(other)]
    Unknown,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn default_on_unexpected_shape<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// Any non-null value is an overlay, if only of unknown kind: the update gates must stop
/// automatic updates on a modified `/usr`.
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
