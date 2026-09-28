//! Process exit codes.

use std::process::ExitCode;

/// How `apexctl` ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exit {
    /// 0: done.
    Ok,
    /// 1: an error, including `profile render --check` finding a difference and a
    /// command-line usage error (clap's own default would be 2, which is taken).
    Error,
    /// 2: an update gate refused the operation (`PolicyDenied`).
    PolicyDenied,
    /// 3: there was nothing to do (nothing staged, already applied, no update).
    NothingToDo,
}

impl Exit {
    /// The numeric exit status.
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
            Self::PolicyDenied => 2,
            Self::NothingToDo => 3,
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        Self::from(exit.code())
    }
}
