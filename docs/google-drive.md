# Google Drive backend

Implemented in [`src/remote/gdrive/`](../src/remote/gdrive/) and covered by
[`tests/gdrive_mock.rs`](../tests/gdrive_mock.rs). **Not yet wired to a mount:**
the OAuth consent flow is missing, and it has never run against real Drive.

## Why it looks the way it does

Drive differs from WebDAV in five ways that are expensive to retrofit. Each one
is why some type in the seam is less obvious than it could be:

| Drive property | Consequence |
|---|---|
| files are addressed by opaque id | `NodeId` is opaque; `root()` is a method, and `child_id()` returns `None` |
| two files in a folder may share a name | `Capabilities::unique_names = false`; the cache has no `UNIQUE(parent, name)` |
| no per-folder ETag | `dir_validators = false`; freshness comes from the change feed |
| change detection is one account-global cursor | `ChangeFeed::Cursor` + `poll_changes` |
| Google-native docs have no size until exported | `SizeHint::Unknown` — reporting `0` is what makes Docs unreadable through rclone's mount |

## Implementation notes

Hand-written against `reqwest` rather than the generated `google-drive3` crate.
The surface actually needed is six endpoints of plain JSON, every request sends
a narrow `fields=` projection anyway, and `google-apis-rs` is in
maintainer-wanted status with a twice-a-year regeneration cadence — not where a
storage backend's contract should live.

Details worth not rediscovering:

- **`size` arrives as a string**, not a number, and is absent for native docs.
- **Both a rate limit and a permission failure are HTTP 403.** Only the
  `errors[].reason` field distinguishes them. `dailyLimitExceeded` is *not*
  retryable: the quota resets at midnight Pacific.
- **`trashed` files are still returned** unless the query excludes them, and a
  file moved to the trash arrives on the change feed as an update, not a
  removal — it is mapped to `Change::Removed`.
- **`newStartPageToken` appears only on the last page** of a change listing.
- **An expired page token** (`invalidPageToken`) means the delta stream is
  broken and the caller must resync; it maps to `RemoteError::Changed`.
- **Export ignores `Range`.** A native document is fetched whole.
- **Forms, Sites and Maps have no export at all** — rejected explicitly rather
  than handed to the user as an unreadable file.
- Requests are paced client-side at ~10/s with a burst of 100, matching rclone's
  empirically-derived defaults. The published quotas are not the binding
  constraint; an undocumented per-second throttle is.

## What is still missing

- **The OAuth consent flow.** Needs a loopback redirect with PKCE, and a
  `setup-gdrive` CLI command. The out-of-band flow is dead.
- **Per-user OAuth client ID.** `drive.readonly` and `drive` are *restricted*
  scopes; a published app needs a CASA assessment (~$500–4500/yr), which no
  hobby project passes. Each user must create their own Google Cloud project and
  OAuth client, as rclone now requires — its shared client id is being retired
  during 2026.
- **Testing against real Drive.** See
  [issue #1](https://github.com/RonnyPfannschmidt/dav-fuse-sync-control/issues/1):
  there is no Drive emulator, service accounts have had a 0 GB storage quota
  since 2023-06-01, and the fixtures in `tests/fixtures/gdrive/` are synthetic
  until a credential exists.

Note that an app whose OAuth consent screen is set to **Internal** inside a
Google Workspace organisation needs no verification at all — no CASA, no
unverified-app warning, no 100-user cap, no 7-day refresh-token expiry. That is
the cheapest route to a working CI credential.
