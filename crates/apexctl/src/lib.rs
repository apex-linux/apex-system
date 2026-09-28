//! `apexctl`, the Apex command-line tool, as a library so that every part is testable.
//!
//! The binary (`src/main.rs`) only parses arguments, runs I/O and prints. The command
//! surface is contract §7 of `docs/specs/M1-IMPLEMENTATION.md`:
//!
//! | Command | Module |
//! |---|---|
//! | `status [--json]` | [`status`] (formatting), [`service`] (D-Bus client) |
//! | `update check\|download\|apply\|discard\|rollback\|changes\|acknowledge\|automatic` | [`update`] |
//! | `update channel stable\|testing` | [`update`] |
//! | `hw report [--json]` | [`hw::report`] |
//! | `hw audio snapshot [--proc FILE] [--json]` | [`hw::audio`], [`codec`] |
//! | `profile render [--profile P] [--out DIR] [--check]` | [`profile`], [`render`], [`scale`], [`patch`] |
//! | `build manifest --root … --version … --revision … --base … [--degraded X]…` | [`manifest`] |
//!
//! [`hda`] holds the HD-audio verb encodings shared by the renderer, the patch-file
//! parser and the audio diagnostics. Exit codes are in [`exit`].

#![warn(missing_docs)]

pub mod cli;
pub mod codec;
pub mod exit;
pub mod hda;
pub mod hw;
pub mod manifest;
pub mod patch;
pub mod profile;
pub mod render;
pub mod scale;
pub mod service;
pub mod status;
pub mod timefmt;
pub mod update;
