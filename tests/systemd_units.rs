//! The shipped systemd units, checked against the code.
//!
//! Pure text assertions: no systemd needed, so these run everywhere. They exist
//! because every one of them encodes something that is silently catastrophic
//! rather than loudly broken.

use std::path::PathBuf;

fn unit(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("systemd")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The service unit with comments stripped.
///
/// The file deliberately *documents* the directives that must not be used, so
/// asserting against the raw text would match the warnings rather than any
/// actual configuration.
fn service() -> String {
    directives(&unit("davfs-sync@.service"))
}

fn directives(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A drift here means a mount with a wrong password restart-loops against the
/// server until Nextcloud's brute-force protection locks the account.
#[test]
fn restart_prevent_exit_status_matches_the_code() {
    let text = service();
    let expected = format!(
        "RestartPreventExitStatus={}",
        davfs_sync::exit::restart_prevent_exit_status()
    );
    assert!(
        text.contains(&expected),
        "the unit file and src/exit.rs disagree; expected {expected:?}"
    );
}

/// `fusermount3` is setuid-root. Under `NoNewPrivileges=yes` the kernel
/// silently ignores the setuid bit and every mount fails with EPERM.
#[test]
fn no_new_privileges_is_off() {
    let text = service();
    assert!(
        text.contains("NoNewPrivileges=no"),
        "NoNewPrivileges must be explicitly no, or mounting fails with EPERM"
    );
    assert!(!text.contains("NoNewPrivileges=yes"));
}

/// Any of these gives the unit its own mount namespace. The mount would then
/// succeed and be invisible to the user's shell and file manager -- the single
/// most confusing possible failure.
#[test]
fn no_directive_creates_a_private_mount_namespace() {
    let text = service();
    for forbidden in [
        "ProtectSystem=",
        "ProtectHome=",
        "PrivateTmp=",
        "ReadWritePaths=",
        "PrivateUsers=",
        "RootDirectory=",
        "TemporaryFileSystem=",
    ] {
        assert!(
            !text.contains(forbidden),
            "{forbidden} gives the unit a private mount namespace, \
             which hides the mount from everything else"
        );
    }
    assert!(
        text.contains("PrivateMounts=no"),
        "PrivateMounts=no should be explicit so nobody re-adds sandboxing"
    );
}

/// This is a cache-backed filesystem: it is supposed to come up offline.
#[test]
fn the_unit_does_not_wait_for_the_network() {
    let text = service();
    assert!(
        !text.contains("After=network-online.target"),
        "waiting for the network defeats an offline-first filesystem"
    );
    assert!(!text.contains("Wants=network-online.target"));
}

/// `Type=notify` already delivers SIGTERM. An `ExecStop=` would spawn a second
/// process racing the first for the same mount point.
#[test]
fn shutdown_is_handled_in_process() {
    let text = service();
    assert!(text.contains("Type=notify"));
    assert!(text.contains("NotifyAccess=main"));
    assert!(
        !text.contains("\nExecStop="),
        "ExecStop= races the main process for the mount point"
    );
    // ExecStopPost is fine: it is the crash path only, and is prefixed with '-'.
    assert!(text.contains("ExecStopPost=-"));
}

/// SIGHUP's default disposition is terminate, so an `ExecReload` that sends it
/// would kill the daemon. We handle it, but the unit still must not claim a
/// reload capability that does nothing.
#[test]
fn there_is_no_execreload_sending_sighup() {
    assert!(
        !service().contains("ExecReload="),
        "no reloadable state exists yet; an ExecReload would be misleading"
    );
}

#[test]
fn restart_is_bounded() {
    let text = service();
    assert!(text.contains("Restart=on-failure"));
    // StartLimit* belong in [Unit], not [Service].
    let unit_section = text.split("[Service]").next().unwrap();
    assert!(
        unit_section.contains("StartLimitIntervalSec="),
        "StartLimitIntervalSec must be in [Unit]"
    );
    assert!(
        unit_section.contains("StartLimitBurst="),
        "StartLimitBurst must be in [Unit]"
    );
}

/// Credentials are per-mount, so `LoadCredentialEncrypted=` belongs in the
/// drop-in: a missing `.cred` file makes the unit fail to start, which would
/// break every Secret Service user.
#[test]
fn the_template_does_not_force_systemd_credentials() {
    let text = service();
    assert!(!text.contains("LoadCredentialEncrypted="));
    assert!(!text.contains("LoadCredential="));
}

#[test]
fn the_watchdog_is_configured() {
    assert!(service().contains("WatchdogSec="));
}

#[test]
fn the_target_exists_and_the_template_joins_it() {
    let target = directives(&unit("davfs-sync.target"));
    assert!(target.contains("[Install]"));
    assert!(target.contains("WantedBy=default.target"));

    let text = service();
    assert!(text.contains("PartOf=davfs-sync.target"));
    assert!(text.contains("WantedBy=davfs-sync.target"));
}

/// The generated drop-in must not reintroduce what the unit carefully avoids.
#[test]
fn the_generated_dropin_carries_no_sandboxing() {
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("DAVFS_URL".to_string(), "https://example.com/".to_string());
    let rendered = davfs_sync::dropin::render(&vars);

    for forbidden in [
        "ReadWritePaths",
        "ProtectSystem",
        "ProtectHome",
        "PrivateTmp",
        "NoNewPrivileges",
    ] {
        assert!(!rendered.contains(forbidden), "{forbidden} must not appear");
    }
}
