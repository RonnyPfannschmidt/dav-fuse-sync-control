//! Process exit codes.
//!
//! These are load-bearing: the unit file's `RestartPreventExitStatus=` keys on
//! them, so a typo here means a mount with a wrong password restart-loops
//! forever and trips Nextcloud's brute-force protection. Hence the tests.
//!
//! Values follow `sysexits.h`, which is what systemd documentation assumes.

/// Everything worked.
pub const OK: u8 = 0;
/// Bad command line.
pub const USAGE: u8 = 64;
/// The mount point is busy, or the mount itself failed.
pub const UNAVAILABLE: u8 = 69;
/// Credentials were rejected. Retrying cannot help.
pub const NOPERM: u8 = 77;
/// Configuration is missing or unusable. Retrying cannot help.
pub const CONFIG: u8 = 78;

/// Exit codes systemd must not restart on.
///
/// Kept next to the constants so the unit file and the code cannot drift.
pub const NO_RESTART: [u8; 2] = [NOPERM, CONFIG];

/// The `RestartPreventExitStatus=` value for the unit file.
pub fn restart_prevent_exit_status() -> String {
    NO_RESTART
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map an error to the code that describes it.
pub fn code_for(err: &anyhow::Error) -> u8 {
    if let Some(remote) = err.downcast_ref::<crate::error::RemoteError>() {
        return match remote {
            crate::error::RemoteError::Unauthorized
            | crate::error::RemoteError::Forbidden
            | crate::error::RemoteError::AuthExpired => NOPERM,
            _ => UNAVAILABLE,
        };
    }
    UNAVAILABLE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RemoteError;

    #[test]
    fn auth_and_config_failures_must_not_be_retried() {
        // If these drift from the unit file, a bad password becomes an infinite
        // restart loop against the server.
        assert!(NO_RESTART.contains(&NOPERM));
        assert!(NO_RESTART.contains(&CONFIG));
        assert!(!NO_RESTART.contains(&UNAVAILABLE));
        assert!(!NO_RESTART.contains(&OK));
    }

    #[test]
    fn the_unit_file_string_matches_the_constants() {
        assert_eq!(restart_prevent_exit_status(), "77 78");
    }

    #[test]
    fn rejected_credentials_map_to_noperm() {
        assert_eq!(
            code_for(&anyhow::Error::new(RemoteError::Unauthorized)),
            NOPERM
        );
        assert_eq!(
            code_for(&anyhow::Error::new(RemoteError::Forbidden)),
            NOPERM
        );
        assert_eq!(
            code_for(&anyhow::Error::new(RemoteError::AuthExpired)),
            NOPERM
        );
    }

    #[test]
    fn a_transient_failure_is_retryable() {
        assert_eq!(
            code_for(&anyhow::Error::new(RemoteError::Offline("down".into()))),
            UNAVAILABLE
        );
        assert!(!NO_RESTART.contains(&UNAVAILABLE));
    }

    #[test]
    fn codes_are_distinct() {
        let all = [OK, USAGE, UNAVAILABLE, NOPERM, CONFIG];
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(all.len(), unique.len());
    }
}
