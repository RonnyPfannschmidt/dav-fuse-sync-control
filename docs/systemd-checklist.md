# systemd checklist

Run before each release, on a real desktop session. `scripts/systemd-smoke.sh`
automates most of it.

Deliberately not in CI. The things that actually break — mount-namespace
visibility to *your* file manager, PAM keyring unlock, TPM access from a
lingering user manager, `fusermount3`'s setuid behaviour under the host's LSM
policy — are precisely what a privileged container fakes. A green check there
would prove very little.

## Install

```bash
mkdir -p ~/.config/systemd/user
cp systemd/davfs-sync@.service systemd/davfs-sync.target ~/.config/systemd/user/
cargo build --release && sudo install -m0755 target/release/davfs-sync /usr/bin/
systemctl --user daemon-reload
```

## Checks

- [ ] `davfs-sync setup <name> …` writes
      `~/.config/systemd/user/davfs-sync@<name>.service.d/50-davfs-sync.conf`
      and `daemon-reload` succeeds.
- [ ] `systemctl --user start davfs-sync@<name>` reaches **`active (running)`**,
      not `activating`. Anything else means `Type=notify` readiness is broken.
- [ ] **The mount is visible outside the service.** Run `ls ~/Cloud/<name>` in a
      plain shell. *This is the check that matters most* — if a `Protect*` or
      `PrivateMounts` directive ever creeps into the unit, the mount succeeds
      inside a private namespace and is invisible to everything else.
- [ ] `cat` a file through the mount.
- [ ] `getfattr -d -m 'user\.davfs\.' <file>` reports state and status.
- [ ] Nautilus shows the mount and the `user.davfs.state` emblem.
- [ ] Readiness fires *after* the mount, not before:
      `journalctl --user -u davfs-sync@<name> | grep mounted`, and
      `systemd-analyze --user blame` shows a short start time even on a large
      server (the crawl happens after `READY=1`).
- [ ] `systemctl --user stop` unmounts cleanly — no
      `Transport endpoint is not connected` left behind.
- [ ] `kill -9 $MAINPID` → `ExecStopPost` clears the mount point, and a restart
      succeeds.
- [ ] Wrong password → unit fails with exit code **77** and does **not**
      restart-loop (`systemctl --user status` shows the limit).
- [ ] Missing `DAVFS_URL` → exit code **78**, same.
- [ ] Watchdog: `fusermount3 -u` the mount behind the daemon's back; systemd
      should kill and restart it within roughly `WatchdogSec`.
- [ ] Reboot with `enable --now` → the mount returns, **and returns offline**.
      Test with the network down: it must still mount and serve cached data.
- [ ] `loginctl enable-linger` + reboot: does credential resolution still work?
      - Secret Service: expect `STATUS=keyring locked — serving from cache`,
        with the mount still up.
      - systemd-creds: expect full function. This is the case that justifies it.

## Known-good divergences from spec/02

`spec/02-architecture.md` sketches a hardened unit. Three of its directives are
actively harmful here and are deliberately absent — `tests/systemd_units.rs`
asserts they stay absent:

| Directive | Why not |
|---|---|
| `NoNewPrivileges=yes` | `fusermount3` is setuid-root; the kernel silently ignores the setuid bit and every mount fails with `EPERM`. |
| `ProtectSystem=` / `ProtectHome=` / `PrivateTmp=` / `ReadWritePaths=` | Each gives the unit its own mount namespace, so the mount exists only inside it — invisible to your shell and file manager. |
| `After=network-online.target` | This is an offline-first cache-backed filesystem; waiting for the network defeats its purpose. In the *user* manager the target is inert anyway. |
