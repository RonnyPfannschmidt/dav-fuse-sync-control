//! Turning backend names into names a filesystem can show.
//!
//! Computed once, when a listing is written to the cache, and persisted. Never
//! recomputed at `readdir` time: the disambiguation below depends on the whole
//! sibling set, so recomputing per call would make `readdir` and `lookup`
//! disagree the moment a sibling is added or removed.
//!
//! Done in the core rather than in each backend, so WebDAV pays nothing and any
//! future backend gets it for free.

use crate::remote::{NodeKind, RemoteEntry};

/// Replacement for `/` in a name. Drive permits it in filenames; a filesystem
/// cannot represent it. U+2215 DIVISION SLASH looks nearly identical.
const SLASH_SUB: char = '\u{2215}';
/// Replacement for a name that is exactly `.` or `..`, which the kernel owns.
const DOT_SUB: char = '\u{FF0E}';

/// Longest name most Linux filesystems accept, in bytes.
const MAX_NAME_BYTES: usize = 255;

/// Length of the id-derived discriminator used to break ties.
const DISCRIMINATOR_LEN: usize = 6;

/// Export extension for Google-native types.
///
/// ODF rather than rclone's OOXML default, since this is a GNOME-targeted
/// project. Configurable per mount later; the mapping lives here so the naming
/// and the export path cannot disagree.
pub fn export_extension(mime: Option<&str>) -> Option<&'static str> {
    match mime? {
        "application/vnd.google-apps.document" => Some("odt"),
        "application/vnd.google-apps.spreadsheet" => Some("ods"),
        "application/vnd.google-apps.presentation" => Some("odp"),
        "application/vnd.google-apps.drawing" => Some("svg"),
        "application/vnd.google-apps.script" => Some("json"),
        _ => None,
    }
}

/// Make one name representable, ignoring siblings.
pub fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| *c != '\0')
        .map(|c| if c == '/' { SLASH_SUB } else { c })
        .collect();

    let cleaned = match cleaned.as_str() {
        "." => DOT_SUB.to_string(),
        ".." => format!("{DOT_SUB}{DOT_SUB}"),
        // A name that is entirely empty cannot be shown at all.
        "" => "unnamed".to_string(),
        _ => cleaned,
    };

    truncate_bytes(&cleaned, MAX_NAME_BYTES)
}

/// Truncate to at most `max` bytes without splitting a character.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Split a name into stem and extension, so a discriminator can be inserted
/// before the extension rather than after it.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        // A leading dot is part of the name (".bashrc"), not an extension.
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Insert an id-derived discriminator: `report.pdf` -> `report~1a2b3c.pdf`.
fn discriminate(name: &str, id: &str) -> String {
    let token: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(DISCRIMINATOR_LEN)
        .collect();
    let token = if token.is_empty() {
        "dup".to_string()
    } else {
        token
    };

    let (stem, ext) = split_extension(name);
    truncate_bytes(&format!("{stem}~{token}{ext}"), MAX_NAME_BYTES)
}

/// Compute display names for one directory's children.
///
/// Returns a name per entry, positionally. Every member of a colliding group is
/// suffixed, including the first: numbering (` (2)`, ` (3)`) renumbers every
/// sibling when one is deleted, which churns inodes and breaks open handles and
/// file-manager selections. An id-derived suffix is stable for the life of the
/// file and independent of what else is in the folder.
pub fn display_names(children: &[RemoteEntry]) -> Vec<String> {
    let base: Vec<String> = children
        .iter()
        .map(|e| {
            let mut name = sanitise(&e.name);
            if e.kind == NodeKind::ExportOnly {
                if let Some(ext) = export_extension(e.mime.as_deref()) {
                    if !name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
                        name = truncate_bytes(&format!("{name}.{ext}"), MAX_NAME_BYTES);
                    }
                }
            }
            name
        })
        .collect();

    // Which base names occur more than once?
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in &base {
        *counts.entry(n.as_str()).or_insert(0) += 1;
    }

    base.iter()
        .zip(children)
        .map(|(name, entry)| {
            if counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
                discriminate(name, entry.id.as_str())
            } else {
                name.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{NodeId, SizeHint};

    fn entry(id: &str, name: &str) -> RemoteEntry {
        RemoteEntry::file(NodeId(id.to_string()), name, 0)
    }

    fn export(id: &str, name: &str, mime: &str) -> RemoteEntry {
        RemoteEntry {
            kind: NodeKind::ExportOnly,
            size: SizeHint::Unknown,
            mime: Some(mime.to_string()),
            ..entry(id, name)
        }
    }

    #[test]
    fn ordinary_names_are_untouched() {
        let names = display_names(&[entry("1", "report.pdf"), entry("2", "notes.md")]);
        assert_eq!(names, vec!["report.pdf", "notes.md"]);
    }

    #[test]
    fn slashes_and_dot_names_become_representable() {
        // Drive permits all three; a filesystem does not.
        assert_eq!(sanitise("a/b"), "a\u{2215}b");
        assert_eq!(sanitise("."), "\u{FF0E}");
        assert_eq!(sanitise(".."), "\u{FF0E}\u{FF0E}");
        assert_eq!(sanitise(""), "unnamed");
        assert!(!sanitise("nul\0byte").contains('\0'));
        // A leading dot is a normal hidden file, not a special name.
        assert_eq!(sanitise(".bashrc"), ".bashrc");
    }

    #[test]
    fn long_names_truncate_on_a_character_boundary() {
        let long = "ä".repeat(200); // 400 bytes
        let out = sanitise(&long);
        assert!(out.len() <= 255);
        // Still valid UTF-8 with no replacement characters -- i.e. not split
        // through the middle of a character.
        assert!(!out.contains('\u{FFFD}'));
        assert!(out.chars().all(|c| c == 'ä'));
    }

    #[test]
    fn duplicates_all_get_a_suffix_so_none_is_renamed_later() {
        let names = display_names(&[
            entry("1a2b3c4d", "report.pdf"),
            entry("9f8e7d6c", "report.pdf"),
        ]);
        assert_eq!(names, vec!["report~1a2b3c.pdf", "report~9f8e7d.pdf"]);
        // The discriminator goes before the extension, so the file still opens
        // with the right application.
        assert!(names.iter().all(|n| n.ends_with(".pdf")));
        assert_ne!(names[0], names[1]);
    }

    /// Adding a third duplicate must not rename the existing two.
    #[test]
    fn adding_a_sibling_does_not_rename_the_others() {
        let two = display_names(&[entry("aaaaaa", "f.txt"), entry("bbbbbb", "f.txt")]);
        let three = display_names(&[
            entry("aaaaaa", "f.txt"),
            entry("bbbbbb", "f.txt"),
            entry("cccccc", "f.txt"),
        ]);
        assert_eq!(two[0], three[0]);
        assert_eq!(two[1], three[1]);
        // ...which is exactly what numbering (" (2)", " (3)") would break.
    }

    /// Removing one duplicate must not rename the survivor's peers either.
    #[test]
    fn removing_a_duplicate_leaves_the_others_alone() {
        let three = display_names(&[
            entry("aaaaaa", "f.txt"),
            entry("bbbbbb", "f.txt"),
            entry("cccccc", "f.txt"),
        ]);
        let two = display_names(&[entry("aaaaaa", "f.txt"), entry("cccccc", "f.txt")]);
        assert_eq!(three[0], two[0]);
        assert_eq!(three[2], two[1]);
    }

    #[test]
    fn a_name_that_collides_only_after_sanitising_is_still_disambiguated() {
        // "a/b" and "a\u{2215}b" both sanitise to the same thing.
        let names = display_names(&[entry("111111", "a/b"), entry("222222", "a\u{2215}b")]);
        assert_ne!(names[0], names[1], "collision survived sanitising");
    }

    #[test]
    fn export_only_nodes_gain_an_extension() {
        let names = display_names(&[
            export("1", "Meeting Notes", "application/vnd.google-apps.document"),
            export("2", "Budget", "application/vnd.google-apps.spreadsheet"),
            export("3", "Deck", "application/vnd.google-apps.presentation"),
        ]);
        assert_eq!(names, vec!["Meeting Notes.odt", "Budget.ods", "Deck.odp"]);
    }

    #[test]
    fn an_export_extension_is_not_doubled() {
        let names = display_names(&[export(
            "1",
            "Report.odt",
            "application/vnd.google-apps.document",
        )]);
        assert_eq!(names, vec!["Report.odt"]);
    }

    #[test]
    fn plain_files_never_gain_an_export_extension() {
        let names = display_names(&[entry("1", "photo.jpg")]);
        assert_eq!(names, vec!["photo.jpg"]);
    }

    #[test]
    fn unknown_google_types_are_left_alone() {
        // Forms and Sites have no export; inventing an extension would lie.
        assert_eq!(
            export_extension(Some("application/vnd.google-apps.form")),
            None
        );
        assert_eq!(export_extension(Some("text/plain")), None);
        assert_eq!(export_extension(None), None);
    }

    #[test]
    fn extension_splitting_handles_edge_cases() {
        assert_eq!(split_extension("a.txt"), ("a", ".txt"));
        assert_eq!(split_extension("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_extension("noext"), ("noext", ""));
        assert_eq!(split_extension(".bashrc"), (".bashrc", ""));
    }
}
