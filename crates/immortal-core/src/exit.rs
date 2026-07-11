//! Stable application exit classes based on BSD `sysexits(3)` conventions.

use std::process::ExitCode;

/// Process exit classification shared by every Immortal executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitClass {
    /// Operation completed successfully (`0`).
    Success,
    /// Command syntax or typed dispatch was invalid (`64`, `EX_USAGE`).
    Usage,
    /// Input data or protocol frame was malformed (`65`, `EX_DATAERR`).
    Data,
    /// Requested file or service does not exist (`66`, `EX_NOINPUT`).
    NotFound,
    /// Required service or capability is unavailable (`69`, `EX_UNAVAILABLE`).
    Unavailable,
    /// Internal invariant or implementation failed (`70`, `EX_SOFTWARE`).
    Software,
    /// Operating-system facility failed (`71`, `EX_OSERR`).
    OsError,
    /// Runtime path or output could not be created (`73`, `EX_CANTCREAT`).
    CantCreate,
    /// Input/output operation failed (`74`, `EX_IOERR`).
    IoError,
    /// Retryable conflict or deadline elapsed (`75`, `EX_TEMPFAIL`).
    TemporaryFailure,
    /// Peer or filesystem permissions rejected the operation (`77`, `EX_NOPERM`).
    Permission,
    /// Service configuration is invalid (`78`, `EX_CONFIG`).
    Configuration,
    /// A multi-service request only partially succeeded (`79`, Immortal extension).
    PartialFailure,
}

impl ExitClass {
    /// Stable numeric status returned to the operating system.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Usage => 64,
            Self::Data => 65,
            Self::NotFound => 66,
            Self::Unavailable => 69,
            Self::Software => 70,
            Self::OsError => 71,
            Self::CantCreate => 73,
            Self::IoError => 74,
            Self::TemporaryFailure => 75,
            Self::Permission => 77,
            Self::Configuration => 78,
            Self::PartialFailure => 79,
        }
    }

    /// Convert into [`ExitCode`] without platform-dependent truncation.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.value())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::ExitClass;

    #[test]
    fn values_are_stable_and_unique() {
        let classes = [
            ExitClass::Success,
            ExitClass::Usage,
            ExitClass::Data,
            ExitClass::NotFound,
            ExitClass::Unavailable,
            ExitClass::Software,
            ExitClass::OsError,
            ExitClass::CantCreate,
            ExitClass::IoError,
            ExitClass::TemporaryFailure,
            ExitClass::Permission,
            ExitClass::Configuration,
            ExitClass::PartialFailure,
        ];
        let values: BTreeSet<u8> = classes.into_iter().map(ExitClass::value).collect();
        assert_eq!(values.len(), classes.len());
        assert_eq!(ExitClass::Success.value(), 0);
        assert_eq!(ExitClass::Configuration.value(), 78);
        assert_eq!(ExitClass::PartialFailure.value(), 79);
    }
}
