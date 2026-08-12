//! PROPFIND request bodies and multistatus parsing.
//!
//! Pure: no `self`, no network, no URL policy in the parser. That is what makes
//! it testable against captured fixtures from real servers.
//!
//! # Why this was rewritten
//!
//! The previous implementation did `xml.split("<d:response>")` and then scanned
//! the result with `str::contains`, line by line. Verified against
//! `rclone serve webdav`, that yields **zero entries**, for two independent
//! reasons:
//!
//! 1. rclone emits `<D:response>` -- uppercase. The split never matches.
//! 2. rclone emits the entire document on a single line, so even with the case
//!    fixed, every property of every resource is harvested from one giant line
//!    and smeared across all of them.
//!
//! Matching on the resolved `DAV:` namespace plus the local name handles `d:`,
//! `D:`, a default namespace, and any other prefix uniformly -- no prefix
//! string appears anywhere below.

use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::NsReader;

use crate::error::RemoteError;

const DAV_NS: &[u8] = b"DAV:";

/// One `<response>` element, still in wire terms (href not yet resolved).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PropfindResponse {
    pub href: String,
    pub is_collection: bool,
    pub content_length: Option<u64>,
    /// Raw HTTP-date string; parsed by the caller.
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub display_name: Option<String>,
    /// Properties the server returned under a non-2xx propstat.
    pub missing_props: Vec<String>,
}

/// Build a PROPFIND request body.
///
/// `getetag` is requested because it is the only per-directory validator we
/// get: a 207 Multi-Status carries no `ETag` *header*, so reading the response
/// header (as the PoC did) always yields `None` and the entire conditional
/// request path is dead.
///
/// The `oc:` properties are Nextcloud extensions; servers that do not know them
/// return them in a 404 propstat, which we discard.
pub fn propfind_body() -> &'static str {
    r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:oc="http://owncloud.org/ns">
  <prop>
    <displayname/>
    <getcontentlength/>
    <getcontenttype/>
    <getlastmodified/>
    <getetag/>
    <resourcetype/>
    <oc:checksums/>
    <oc:id/>
  </prop>
</propfind>"#
}

/// Which element inside a `<prop>` we are currently reading text for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prop {
    None,
    Href,
    DisplayName,
    ContentLength,
    ContentType,
    LastModified,
    Etag,
    Status,
}

/// Parse a `207 Multi-Status` body.
///
/// Properties are only accepted from a `<propstat>` whose `<status>` is 2xx;
/// anything else is recorded in `missing_props`. Without that, a Nextcloud
/// response carrying a second `404 Not Found` propstat for unsupported
/// properties would contribute empty values that look like real data.
pub fn parse_multistatus(xml: &str) -> Result<Vec<PropfindResponse>, RemoteError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out: Vec<PropfindResponse> = Vec::new();
    let mut buf = Vec::new();

    // State for the response currently being built.
    let mut cur: Option<PropfindResponse> = None;
    // Properties of the propstat being read, held aside until we see its status.
    let mut pending: Option<PropfindResponse> = None;
    let mut pending_names: Vec<String> = Vec::new();
    let mut status_text = String::new();
    let mut reading = Prop::None;
    // Text arrives in several events when a value contains character
    // references, so it must be accumulated and committed at the closing tag.
    let mut text_buf = String::new();
    let mut saw_multistatus = false;
    let mut saw_multistatus_end = false;
    let mut depth_in_prop = 0usize;

    loop {
        let (ns, ev) = reader
            .read_resolved_event_into(&mut buf)
            .map_err(|e| RemoteError::Protocol(format!("malformed XML: {e}")))?;

        let in_dav = matches!(ns, ResolveResult::Bound(Namespace(n)) if n == DAV_NS);

        match ev {
            Event::Eof => break,

            Event::Start(ref e) => {
                let local = e.local_name();
                let local = local.as_ref();
                if !in_dav {
                    // A non-DAV element inside <prop> (e.g. oc:checksums).
                    if depth_in_prop > 0 {
                        depth_in_prop += 1;
                    }
                    continue;
                }
                match local {
                    b"multistatus" => saw_multistatus = true,
                    b"response" => {
                        cur = Some(PropfindResponse::default());
                        pending = None;
                        pending_names.clear();
                    }
                    b"propstat" => {
                        pending = Some(PropfindResponse::default());
                        pending_names.clear();
                        status_text.clear();
                    }
                    b"prop" => depth_in_prop = 1,
                    b"href" => reading = Prop::Href,
                    b"status" => reading = Prop::Status,
                    b"displayname" => {
                        pending_names.push("displayname".into());
                        reading = Prop::DisplayName;
                        text_buf.clear();
                    }
                    b"getcontentlength" => {
                        pending_names.push("getcontentlength".into());
                        reading = Prop::ContentLength;
                        text_buf.clear();
                    }
                    b"getcontenttype" => {
                        pending_names.push("getcontenttype".into());
                        reading = Prop::ContentType;
                        text_buf.clear();
                    }
                    b"getlastmodified" => {
                        pending_names.push("getlastmodified".into());
                        reading = Prop::LastModified;
                        text_buf.clear();
                    }
                    b"getetag" => {
                        pending_names.push("getetag".into());
                        reading = Prop::Etag;
                        text_buf.clear();
                    }
                    b"resourcetype" => pending_names.push("resourcetype".into()),
                    // <collection> may appear as Start or Empty.
                    b"collection" => {
                        if let Some(p) = pending.as_mut() {
                            p.is_collection = true;
                        }
                    }
                    _ => {}
                }
                if depth_in_prop > 0 {
                    depth_in_prop += 1;
                }
            }

            Event::Empty(ref e) => {
                if !in_dav {
                    continue;
                }
                let local = e.local_name();
                match local.as_ref() {
                    b"collection" => {
                        if let Some(p) = pending.as_mut() {
                            p.is_collection = true;
                        }
                    }
                    b"displayname" => pending_names.push("displayname".into()),
                    b"getcontentlength" => pending_names.push("getcontentlength".into()),
                    b"getetag" => pending_names.push("getetag".into()),
                    b"getlastmodified" => pending_names.push("getlastmodified".into()),
                    b"resourcetype" => pending_names.push("resourcetype".into()),
                    _ => {}
                }
            }

            Event::Text(ref t) => {
                if reading == Prop::None {
                    continue;
                }
                // Two steps in quick-xml 0.41: xml10_content() does the charset
                // decode and EOL normalisation, escape::unescape() turns &#39;
                // and &amp; back into characters. The old string-matching parser
                // did neither, so "apostrophe's.txt" surfaced to the kernel as
                // "apostrophe&#39;s.txt".
                let decoded = t
                    .xml10_content()
                    .map_err(|e| RemoteError::Protocol(format!("bad encoding: {e}")))?;
                let text = quick_xml::escape::unescape(&decoded)
                    .map_err(|e| RemoteError::Protocol(format!("bad entity: {e}")))?
                    .into_owned();

                text_buf.push_str(&text);
            }

            // `&#39;` / `&amp;` inside a property value. Without this the
            // character is silently dropped: "apostrophe's.txt" became
            // "apostrophes.txt".
            Event::GeneralRef(ref r) => {
                if reading == Prop::None {
                    continue;
                }
                let name = r
                    .decode()
                    .map_err(|e| RemoteError::Protocol(format!("bad encoding: {e}")))?;
                let resolved = quick_xml::escape::unescape(&format!("&{name};"))
                    .map_err(|e| RemoteError::Protocol(format!("unknown entity &{name};: {e}")))?
                    .into_owned();
                text_buf.push_str(&resolved);
            }

            Event::End(ref e) => {
                depth_in_prop = depth_in_prop.saturating_sub(1);
                if !in_dav {
                    continue;
                }
                let local = e.local_name();

                // Commit whatever text we accumulated for the property just closed.
                if reading != Prop::None {
                    let text = std::mem::take(&mut text_buf);
                    match reading {
                        Prop::Href => {
                            if let Some(c) = cur.as_mut() {
                                c.href = text;
                            }
                        }
                        Prop::Status => status_text = text,
                        _ => {
                            if let Some(p) = pending.as_mut() {
                                match reading {
                                    Prop::DisplayName => p.display_name = Some(text),
                                    Prop::ContentLength => {
                                        p.content_length = text.trim().parse().ok()
                                    }
                                    Prop::ContentType => p.content_type = Some(text),
                                    Prop::LastModified => p.last_modified = Some(text),
                                    Prop::Etag => p.etag = Some(text),
                                    _ => {}
                                }
                            }
                        }
                    }
                }

                match local.as_ref() {
                    b"multistatus" => saw_multistatus_end = true,
                    b"propstat" => {
                        let props = pending.take().unwrap_or_default();
                        if status_is_success(&status_text) {
                            if let Some(c) = cur.as_mut() {
                                merge(c, props);
                            }
                        } else if let Some(c) = cur.as_mut() {
                            c.missing_props.append(&mut pending_names);
                        }
                        pending_names.clear();
                    }
                    b"response" => {
                        if let Some(c) = cur.take() {
                            if !c.href.is_empty() {
                                out.push(c);
                            }
                        }
                    }
                    _ => {}
                }
                reading = Prop::None;
            }

            _ => {}
        }
        buf.clear();
    }

    if saw_multistatus && !saw_multistatus_end {
        // We saw the opening tag but the document stopped early: a cut
        // connection or a truncated proxy response. Reporting the partial
        // listing would look like files had been deleted.
        return Err(RemoteError::Protocol(
            "truncated multistatus: no closing element".into(),
        ));
    }

    if !saw_multistatus {
        // A SabreDAV "Propfind-Finite-Depth" 403, an HTML error page, or a
        // truncated body. Returning Ok(vec![]) here would be recorded in the
        // cache as "this directory is empty", which is how a transient server
        // error becomes apparent data loss.
        return Err(RemoteError::Protocol(
            "response is not a DAV: multistatus".into(),
        ));
    }

    Ok(out)
}

/// Copy the properties that were actually present.
fn merge(dst: &mut PropfindResponse, src: PropfindResponse) {
    dst.is_collection |= src.is_collection;
    dst.content_length = dst.content_length.or(src.content_length);
    dst.last_modified = dst.last_modified.take().or(src.last_modified);
    dst.etag = dst.etag.take().or(src.etag);
    dst.content_type = dst.content_type.take().or(src.content_type);
    dst.display_name = dst.display_name.take().or(src.display_name);
}

/// `HTTP/1.1 200 OK` -> true. Anything non-2xx (or unparseable) -> false.
fn status_is_success(status: &str) -> bool {
    status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .map(|c| (200..300).contains(&c))
        // An absent <status> is a malformed propstat; treat the props as good
        // rather than silently dropping a whole directory.
        .unwrap_or(status.trim().is_empty())
}

/// One resolved entry: a decoded path relative to the listed directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedEntry {
    /// Path relative to the base URL, percent-decoded, no leading or trailing
    /// slash. Empty for the directory itself.
    pub path: String,
    /// Final path component, percent-decoded.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<chrono::DateTime<chrono::Utc>>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

/// A directory listing with the self-response separated out.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedListing {
    /// The listed directory's own entry. `Depth: 1` always includes it, and it
    /// is where the directory's validator comes from.
    pub this: Option<ResolvedEntry>,
    pub children: Vec<ResolvedEntry>,
}

/// Turn wire responses into paths relative to `base`.
///
/// Fixes three things the PoC got wrong:
///
/// * The self-response is separated out rather than listed as a child. The old
///   code filtered only on `name != "."`, so every directory appeared inside
///   itself -- verified against rclone, where `PROPFIND /nested/a/b/c/` returns
///   a self-entry with `<displayname>c</displayname>`.
/// * hrefs are resolved against the base URL, so both path-absolute
///   (`/remote.php/dav/files/u/x`) and full-URL hrefs work. The old code did
///   `href.strip_prefix(base_url_str)` -- comparing a full URL against a
///   path-absolute href, which never matches on Nextcloud.
/// * Percent-decoding is lossy-UTF-8 rather than `unwrap_or_default()`, which
///   turned an undecodable sequence into the *empty string* and silently
///   reparented the entry to the root.
///
/// `dir_url` is the URL of the **directory that was listed**, not the account
/// root, and must end in `/`. Passing the account root makes the self-entry of
/// any subdirectory resolve to a non-empty path, at which point it is
/// indistinguishable from a child -- which is the bug this function exists to
/// prevent.
pub fn resolve(dir_url: &url::Url, responses: &[PropfindResponse]) -> ResolvedListing {
    let base = dir_url;
    let mut listing = ResolvedListing::default();

    for r in responses {
        let Ok(abs) = url::Url::options().base_url(Some(base)).parse(&r.href) else {
            tracing::debug!("skipping unparseable href {:?}", r.href);
            continue;
        };

        // Only entries at or below the base belong to this listing.
        let Some(rel) = base.make_relative(&abs) else {
            tracing::debug!("href {:?} is not below base {}", r.href, base);
            continue;
        };

        let rel = rel.trim_end_matches('/');
        // A parent-relative result means the server returned something above
        // the base; not ours to interpret.
        if rel.starts_with("..") {
            continue;
        }

        let path = percent_decode(rel);
        let name = path.rsplit('/').next().unwrap_or_default().to_string();

        let entry = ResolvedEntry {
            is_dir: r.is_collection,
            size: r.content_length.unwrap_or(0),
            mtime: r.last_modified.as_deref().and_then(parse_http_date),
            etag: r.etag.clone(),
            content_type: r.content_type.clone(),
            name,
            path,
        };

        if entry.path.is_empty() {
            listing.this = Some(entry);
        } else {
            listing.children.push(entry);
        }
    }

    listing
}

/// Percent-decode a path, preserving `/` boundaries.
fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Parse an HTTP-date (RFC 1123 / RFC 2822) into a UTC timestamp.
///
/// Accepts both `GMT` and numeric-offset spellings; servers emit each.
pub fn parse_http_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc2822(s)
        .ok()
        .or_else(|| {
            // RFC 2822 parsing rejects "GMT" in some chrono versions; retry
            // with the equivalent numeric offset.
            chrono::DateTime::parse_from_rfc2822(&s.replace("GMT", "+0000")).ok()
        })
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_non_multistatus_body_instead_of_reporting_an_empty_directory() {
        // SabreDAV returns this for Depth: infinity. Ok(vec![]) would be cached
        // as "the directory is empty".
        let err = parse_multistatus(
            r#"<?xml version="1.0"?><d:error xmlns:d="DAV:"><s:exception>Sabre\DAV\Exception\RequestedRangeNotSatisfiable</s:exception></d:error>"#,
        );
        assert!(matches!(err, Err(RemoteError::Protocol(_))));

        assert!(parse_multistatus("<html><body>502 Bad Gateway</body></html>").is_err());
        assert!(parse_multistatus("").is_err());
    }

    #[test]
    fn truncated_xml_is_an_error() {
        let s = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/a"#;
        assert!(parse_multistatus(s).is_err());
    }

    /// The same document in four namespace spellings must parse identically.
    #[test]
    fn namespace_prefix_is_irrelevant() {
        let variants = [
            // lowercase prefix
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response><d:href>/f.txt</d:href><d:propstat><d:prop><d:getcontentlength>7</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
            // uppercase prefix -- what rclone actually sends
            r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/f.txt</D:href><D:propstat><D:prop><D:getcontentlength>7</D:getcontentlength><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#,
            // default namespace, no prefix at all
            r#"<?xml version="1.0"?><multistatus xmlns="DAV:"><response><href>/f.txt</href><propstat><prop><getcontentlength>7</getcontentlength><resourcetype/></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#,
            // an arbitrary third prefix
            r#"<?xml version="1.0"?><ns0:multistatus xmlns:ns0="DAV:"><ns0:response><ns0:href>/f.txt</ns0:href><ns0:propstat><ns0:prop><ns0:getcontentlength>7</ns0:getcontentlength><ns0:resourcetype/></ns0:prop><ns0:status>HTTP/1.1 200 OK</ns0:status></ns0:propstat></ns0:response></ns0:multistatus>"#,
        ];

        for (i, v) in variants.iter().enumerate() {
            let r = parse_multistatus(v).unwrap_or_else(|e| panic!("variant {i} failed: {e}"));
            assert_eq!(r.len(), 1, "variant {i}");
            assert_eq!(r[0].href, "/f.txt", "variant {i}");
            assert_eq!(r[0].content_length, Some(7), "variant {i}");
            assert!(!r[0].is_collection, "variant {i}");
        }
    }

    #[test]
    fn indentation_is_irrelevant() {
        let one_line = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/a.txt</D:href><D:propstat><D:prop><D:getcontentlength>3</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let indented = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/a.txt</D:href>
    <D:propstat>
      <D:prop>
        <D:getcontentlength>3</D:getcontentlength>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        assert_eq!(
            parse_multistatus(one_line).unwrap(),
            parse_multistatus(indented).unwrap()
        );
    }

    #[test]
    fn collection_detected_as_start_or_empty_element() {
        for rt in [
            "<D:resourcetype><D:collection/></D:resourcetype>",
            "<D:resourcetype><D:collection></D:collection></D:resourcetype>",
            // rclone redeclares the namespace inline on the collection element.
            r#"<D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype>"#,
        ] {
            let xml = format!(
                r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/d/</D:href><D:propstat><D:prop>{rt}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
            );
            assert!(parse_multistatus(&xml).unwrap()[0].is_collection, "{rt}");
        }
    }

    #[test]
    fn empty_resourcetype_means_a_regular_file() {
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/f</D:href><D:propstat><D:prop><D:resourcetype></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        assert!(!parse_multistatus(xml).unwrap()[0].is_collection);
    }

    #[test]
    fn properties_from_a_404_propstat_are_discarded() {
        // Nextcloud answers unsupported properties in a second propstat.
        // Accepting them would fabricate empty values that look like real data.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/u/a.txt</d:href>
    <d:propstat>
      <d:prop><d:getcontentlength>42</d:getcontentlength><d:getetag>"abc"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><d:displayname/><oc:checksums/></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let r = parse_multistatus(xml).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].content_length, Some(42));
        assert_eq!(r[0].etag.as_deref(), Some("\"abc\""));
        assert_eq!(r[0].display_name, None, "404 propstat must not contribute");
        assert!(r[0].missing_props.iter().any(|p| p == "displayname"));
    }

    #[test]
    fn xml_entities_are_decoded() {
        // The old parser handed these through raw.
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/apostrophe%27s.txt</D:href><D:propstat><D:prop><D:displayname>apostrophe&#39;s.txt</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/a%26b</D:href><D:propstat><D:prop><D:displayname>a&amp;b</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let r = parse_multistatus(xml).unwrap();
        assert_eq!(r[0].display_name.as_deref(), Some("apostrophe's.txt"));
        assert_eq!(r[1].display_name.as_deref(), Some("a&b"));
    }

    #[test]
    fn etag_is_read_from_the_body() {
        // A 207 has no ETag *header*, so the body is the only source. Reading
        // the header (as the PoC did) always produced None, which silently
        // disabled every conditional request.
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/d/</D:href><D:propstat><D:prop><D:getetag>"dir-v2"</D:getetag><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(
            parse_multistatus(xml).unwrap()[0].etag.as_deref(),
            Some("\"dir-v2\"")
        );
    }

    /// `resolve` must be given the listed directory, not the account root.
    ///
    /// Caught by a live test against rclone: passing the account root made the
    /// self-entry of every subdirectory resolve to a non-empty path, so `ls
    /// nested/a/b/c` showed a `c` inside itself -- the original bug, reproduced
    /// through a different mistake.
    #[test]
    fn self_entry_is_relative_to_the_listed_directory() {
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/nested/a/b/c/</D:href><D:propstat><D:prop><D:displayname>c</D:displayname><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/nested/a/b/c/deep.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>5</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let responses = parse_multistatus(xml).unwrap();

        let dir_url = url::Url::parse("http://h/nested/a/b/c/").unwrap();
        let listing = resolve(&dir_url, &responses);
        assert!(listing.this.is_some(), "self-entry not identified");
        assert_eq!(listing.children.len(), 1);
        assert_eq!(listing.children[0].name, "deep.txt");

        // The account root instead: the self-entry is no longer at path "" and
        // would be mistaken for a child.
        let root = url::Url::parse("http://h/").unwrap();
        let wrong = resolve(&root, &responses);
        assert!(
            wrong.this.is_none(),
            "sanity check: with the wrong base there is no empty-path entry"
        );
    }

    #[test]
    fn entries_above_the_listed_directory_are_ignored() {
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/other/elsewhere.txt</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let responses = parse_multistatus(xml).unwrap();
        let listing = resolve(&url::Url::parse("http://h/dav/").unwrap(), &responses);
        assert!(listing.children.is_empty(), "escaped the listed directory");
    }

    #[test]
    fn http_dates_parse_in_both_spellings() {
        let gmt = parse_http_date("Wed, 12 Aug 2026 06:37:07 GMT").unwrap();
        let off = parse_http_date("Wed, 12 Aug 2026 06:37:07 +0000").unwrap();
        assert_eq!(gmt, off);
        assert_eq!(gmt.timestamp(), 1_786_516_627);
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("").is_none());
    }

    #[test]
    fn status_classification() {
        assert!(status_is_success("HTTP/1.1 200 OK"));
        assert!(status_is_success("HTTP/1.1 207 Multi-Status"));
        assert!(!status_is_success("HTTP/1.1 404 Not Found"));
        assert!(!status_is_success("HTTP/1.1 403 Forbidden"));
        assert!(!status_is_success("HTTP/1.1 500 Internal Server Error"));
    }

    #[test]
    fn body_requests_getetag() {
        // Regression guard: without getetag there is no directory validator at
        // all, because a 207 carries no ETag header.
        assert!(propfind_body().contains("<getetag/>"));
        assert!(propfind_body().contains("<resourcetype/>"));
    }
}
