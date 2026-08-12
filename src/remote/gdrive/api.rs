//! Narrow deserialisation types for the Drive v3 JSON API.
//!
//! Deliberately not the generated `google-drive3` types. Every request sends a
//! `fields=` projection, so only a handful of properties ever come back; a
//! struct that names exactly those is both smaller and self-documenting about
//! what was asked for. The generated `File` is ~60 all-`Option` fields.

use serde::Deserialize;

/// MIME type marking a folder.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// MIME type marking a shortcut to another file.
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
/// Prefix shared by every Google-native type (Docs, Sheets, ...).
pub const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// The projection sent as `fields=` on file requests.
///
/// `size` and `md5Checksum` are absent for Google-native documents, which is
/// exactly why [`crate::remote::SizeHint`] exists. `version` is present for
/// everything, so it is the fallback validator.
pub const FILE_FIELDS: &str = "id,name,mimeType,parents,size,modifiedTime,createdTime,\
                               md5Checksum,version,trashed,headRevisionId,\
                               shortcutDetails(targetId,targetMimeType)";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Since 2020-09-30 a file has at most one parent, but the API still types
    /// this as an array.
    #[serde(default)]
    pub parents: Vec<String>,
    /// Absent for Google-native documents. A string in the JSON, not a number.
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    /// Only for files with binary content.
    #[serde(default)]
    pub md5_checksum: Option<String>,
    /// "A monotonically increasing version number", present for everything.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub trashed: bool,
    #[serde(default)]
    pub head_revision_id: Option<String>,
    #[serde(default)]
    pub shortcut_details: Option<ShortcutDetails>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutDetails {
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub target_mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    #[serde(default)]
    pub files: Vec<DriveFile>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    /// True when the result set is known to be incomplete because some corpora
    /// could not be searched.
    #[serde(default)]
    pub incomplete_search: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartPageToken {
    #[serde(default)]
    pub start_page_token: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChangeList {
    #[serde(default)]
    pub changes: Vec<DriveChange>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    /// Present only on the last page; becomes the cursor for the next poll.
    #[serde(default)]
    pub new_start_page_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveChange {
    #[serde(default)]
    pub file_id: Option<String>,
    /// True when the file was deleted or is no longer visible to this user.
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub file: Option<DriveFile>,
    #[serde(default)]
    pub change_type: Option<String>,
}

/// The error envelope Drive returns for a failed request.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ErrorEnvelope {
    #[serde(default)]
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ErrorBody {
    #[serde(default)]
    pub code: u16,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub errors: Vec<ErrorDetail>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ErrorDetail {
    /// The field that actually distinguishes a rate limit from a permission
    /// problem: both arrive as HTTP 403.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: String,
}

impl ErrorBody {
    /// The first `reason`, which is what error classification keys on.
    pub fn reason(&self) -> Option<&str> {
        self.errors.first().map(|e| e.reason.as_str())
    }
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type.as_deref() == Some(FOLDER_MIME)
    }

    pub fn is_shortcut(&self) -> bool {
        self.mime_type.as_deref() == Some(SHORTCUT_MIME)
    }

    /// A Google-native document: readable only via `files.export`, and with no
    /// byte size until then.
    pub fn is_export_only(&self) -> bool {
        self.mime_type
            .as_deref()
            .map(|m| m.starts_with(GOOGLE_APPS_PREFIX))
            .unwrap_or(false)
            && !self.is_folder()
            && !self.is_shortcut()
    }

    pub fn parent(&self) -> Option<&str> {
        self.parents.first().map(String::as_str)
    }

    /// `size` arrives as a decimal string; absent for native documents.
    pub fn size_bytes(&self) -> Option<u64> {
        self.size.as_ref().and_then(|s| s.parse().ok())
    }

    pub fn modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        parse_rfc3339(self.modified_time.as_deref()?)
    }

    pub fn created(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        parse_rfc3339(self.created_time.as_deref()?)
    }
}

/// Drive timestamps are RFC 3339, not the RFC 2822 that WebDAV uses.
pub fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Build the `q` parameter listing a folder's children.
///
/// Trashed files are excluded: they are still returned otherwise, and would
/// show up in the mount as live files.
pub fn children_query(parent_id: &str) -> String {
    format!(
        "'{}' in parents and trashed = false",
        escape_query(parent_id)
    )
}

/// Escape a value for a Drive `q` string, where `'` and `\` are special.
pub fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_document_has_no_size() {
        // The property SizeHint::Unknown exists for.
        let json = r#"{
            "id": "1abc", "name": "Notes",
            "mimeType": "application/vnd.google-apps.document",
            "parents": ["root"], "version": "12",
            "modifiedTime": "2026-08-12T06:37:07.000Z"
        }"#;
        let f: DriveFile = serde_json::from_str(json).unwrap();

        assert!(f.is_export_only());
        assert!(!f.is_folder());
        assert_eq!(f.size_bytes(), None, "a native doc must not report a size");
        assert_eq!(f.md5_checksum, None);
        assert_eq!(f.parent(), Some("root"));
    }

    #[test]
    fn a_binary_file_reports_size_and_checksum() {
        let json = r#"{
            "id": "2def", "name": "photo.jpg", "mimeType": "image/jpeg",
            "parents": ["1abc"], "size": "1048576",
            "md5Checksum": "d41d8cd98f00b204e9800998ecf8427e",
            "version": "3", "modifiedTime": "2026-08-12T06:37:07.000Z",
            "headRevisionId": "0Bxyz"
        }"#;
        let f: DriveFile = serde_json::from_str(json).unwrap();

        // The API sends size as a *string*; a u64 field would fail to parse.
        assert_eq!(f.size_bytes(), Some(1_048_576));
        assert!(!f.is_export_only());
        assert_eq!(f.modified().unwrap().timestamp(), 1_786_516_627);
    }

    #[test]
    fn a_folder_is_recognised_by_mime_type() {
        let json = r#"{"id":"f1","name":"Docs",
                       "mimeType":"application/vnd.google-apps.folder","parents":["root"]}"#;
        let f: DriveFile = serde_json::from_str(json).unwrap();
        assert!(f.is_folder());
        assert!(!f.is_export_only(), "a folder is not an export-only doc");
    }

    #[test]
    fn a_shortcut_carries_its_target() {
        let json = r#"{"id":"s1","name":"link",
                       "mimeType":"application/vnd.google-apps.shortcut",
                       "parents":["root"],
                       "shortcutDetails":{"targetId":"2def","targetMimeType":"image/jpeg"}}"#;
        let f: DriveFile = serde_json::from_str(json).unwrap();
        assert!(f.is_shortcut());
        assert!(!f.is_export_only());
        assert_eq!(
            f.shortcut_details.unwrap().target_id.as_deref(),
            Some("2def")
        );
    }

    #[test]
    fn unknown_fields_do_not_break_deserialisation() {
        // The API adds fields over time; a strict struct would start failing.
        let json =
            r#"{"id":"x","name":"n","somethingNew":{"a":1},"capabilities":{"canEdit":true}}"#;
        assert!(serde_json::from_str::<DriveFile>(json).is_ok());
    }

    #[test]
    fn a_minimal_file_deserialises() {
        let f: DriveFile = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(f.id, "x");
        assert_eq!(f.parent(), None);
        assert!(!f.trashed);
    }

    #[test]
    fn timestamps_are_rfc3339_not_rfc2822() {
        assert!(parse_rfc3339("2026-08-12T06:37:07.000Z").is_some());
        assert!(parse_rfc3339("2026-08-12T06:37:07Z").is_some());
        assert!(parse_rfc3339("2026-08-12T08:37:07+02:00").is_some());
        // The WebDAV spelling must not parse here.
        assert!(parse_rfc3339("Wed, 12 Aug 2026 06:37:07 GMT").is_none());
    }

    #[test]
    fn a_change_page_carries_a_new_start_token_only_at_the_end() {
        let mid = r#"{"changes":[{"fileId":"a","removed":false}],"nextPageToken":"p2"}"#;
        let page: ChangeList = serde_json::from_str(mid).unwrap();
        assert_eq!(page.next_page_token.as_deref(), Some("p2"));
        assert_eq!(page.new_start_page_token, None);

        let last = r#"{"changes":[],"newStartPageToken":"999"}"#;
        let page: ChangeList = serde_json::from_str(last).unwrap();
        assert_eq!(page.new_start_page_token.as_deref(), Some("999"));
    }

    #[test]
    fn a_removal_has_no_file_body() {
        let json = r#"{"fileId":"gone","removed":true,"changeType":"file"}"#;
        let c: DriveChange = serde_json::from_str(json).unwrap();
        assert!(c.removed);
        assert!(c.file.is_none());
    }

    /// Both a rate limit and a permission failure arrive as HTTP 403; only
    /// `reason` tells them apart.
    #[test]
    fn the_error_reason_distinguishes_403s() {
        let rate = r#"{"error":{"code":403,"message":"Rate Limit Exceeded",
            "errors":[{"reason":"userRateLimitExceeded","message":"Rate Limit Exceeded"}]}}"#;
        let e: ErrorEnvelope = serde_json::from_str(rate).unwrap();
        assert_eq!(e.error.reason(), Some("userRateLimitExceeded"));

        let perm = r#"{"error":{"code":403,"message":"Insufficient permissions",
            "errors":[{"reason":"insufficientFilePermissions","message":"..."}]}}"#;
        let e: ErrorEnvelope = serde_json::from_str(perm).unwrap();
        assert_eq!(e.error.reason(), Some("insufficientFilePermissions"));

        // A body with no details must not panic.
        let bare: ErrorEnvelope = serde_json::from_str(r#"{"error":{"code":500}}"#).unwrap();
        assert_eq!(bare.error.reason(), None);
    }

    #[test]
    fn queries_exclude_trashed_files() {
        let q = children_query("1abc");
        assert!(q.contains("'1abc' in parents"));
        assert!(
            q.contains("trashed = false"),
            "trashed files would appear as live files in the mount"
        );
    }

    #[test]
    fn query_values_are_escaped() {
        // A crafted id must not be able to break out of the quoted string.
        let q = children_query("a'b");
        assert!(q.contains("a\\'b"), "quote not escaped: {q}");
        assert_eq!(escape_query("a\\b"), "a\\\\b");
    }

    #[test]
    fn the_field_projection_asks_for_what_the_mapping_needs() {
        for field in [
            "id",
            "name",
            "mimeType",
            "parents",
            "size",
            "modifiedTime",
            "md5Checksum",
            "version",
            "trashed",
            "shortcutDetails",
        ] {
            assert!(FILE_FIELDS.contains(field), "{field} missing from fields=");
        }
    }
}
