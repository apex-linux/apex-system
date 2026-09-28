use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exit {
    Ok,
    /// Also for command-line usage errors: clap's default 2 is taken by `PolicyDenied`.
    Error,
    PolicyDenied,
    NothingToDo,
}

impl Exit {
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
