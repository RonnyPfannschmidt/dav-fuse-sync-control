//! Golden tests for the PROPFIND parser against captured server responses.
//!
//! Each `tests/fixtures/propfind/<name>.xml` is paired with `<name>.base`
//! holding the base URL it was captured against. Adding a fixture is a file
//! drop -- no code change. Errors are snapshotted too, so "this server returns
//! garbage" is a documented outcome rather than an untested panic.

use std::path::PathBuf;

use davfs_sync::webdav::propfind::{parse_multistatus, resolve};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/propfind")
}

fn fixtures() -> Vec<(String, String, url::Url)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let xml = std::fs::read_to_string(&path).expect("read fixture");
        let base_raw = std::fs::read_to_string(path.with_extension("base"))
            .unwrap_or_else(|_| panic!("{stem}.base is missing"));
        let base = url::Url::parse(base_raw.trim()).expect("valid base URL");
        out.push((stem, xml, base));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no fixtures found");
    out
}

#[test]
fn golden() {
    for (stem, xml, base) in fixtures() {
        let parsed = parse_multistatus(&xml);
        let resolved = parsed.as_ref().ok().map(|r| resolve(&base, r));
        let rendered = match (&parsed, &resolved) {
            (Ok(_), Some(listing)) => serde_yaml_ish(listing),
            (Err(e), _) => format!("ERROR: {e}"),
            _ => unreachable!(),
        };

        insta::with_settings!({
            snapshot_suffix => stem.clone(),
            description => format!("fixture: {stem}.xml"),
            omit_expression => true,
        }, {
            insta::assert_snapshot!(rendered);
        });
    }
}

/// Render a listing as stable, reviewable text.
fn serde_yaml_ish(listing: &davfs_sync::webdav::propfind::ResolvedListing) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    match &listing.this {
        Some(t) => writeln!(s, "self: {} (dir={})", t.path, t.is_dir).unwrap(),
        None => writeln!(s, "self: <none>").unwrap(),
    }
    if let Some(etag) = listing.this.as_ref().and_then(|t| t.etag.as_deref()) {
        writeln!(s, "self_etag: {etag}").unwrap();
    }
    writeln!(s, "children: {}", listing.children.len()).unwrap();
    for c in &listing.children {
        write!(
            s,
            "  - path={:?} name={:?} dir={} size={}",
            c.path, c.name, c.is_dir, c.size
        )
        .unwrap();
        if let Some(m) = c.mtime {
            write!(s, " mtime={}", m.timestamp()).unwrap();
        }
        if let Some(e) = &c.etag {
            write!(s, " etag={e}").unwrap();
        }
        writeln!(s).unwrap();
    }
    s
}

/// The bug that motivated the rewrite: the previous parser split on the literal
/// string `<d:response>`, so a server using any other prefix yielded nothing.
#[test]
fn every_fixture_that_should_parse_yields_entries() {
    let expect_error = ["sabredav_403_finite_depth", "truncated", "not_xml"];

    for (stem, xml, base) in fixtures() {
        let parsed = parse_multistatus(&xml);
        if expect_error.contains(&stem.as_str()) {
            assert!(parsed.is_err(), "{stem}: expected a parse error");
            continue;
        }
        let responses = parsed.unwrap_or_else(|e| panic!("{stem}: {e}"));
        let listing = resolve(&base, &responses);

        if stem == "empty_collection" {
            assert!(listing.children.is_empty(), "{stem}: expected no children");
            assert!(listing.this.is_some(), "{stem}: self-entry missing");
        } else {
            assert!(
                !listing.children.is_empty(),
                "{stem}: parsed to zero children -- this is the exact failure \
                 the string-matching parser had against rclone"
            );
        }
    }
}

/// A directory must never appear inside itself.
#[test]
fn no_directory_lists_itself() {
    for (stem, xml, base) in fixtures() {
        let Ok(responses) = parse_multistatus(&xml) else {
            continue;
        };
        let listing = resolve(&base, &responses);

        for c in &listing.children {
            assert!(
                !c.path.is_empty(),
                "{stem}: the self-entry leaked into children"
            );
        }
        if let Some(this) = &listing.this {
            assert!(
                !listing
                    .children
                    .iter()
                    .any(|c| c.name == this.name && c.is_dir && c.path == this.name),
                "{stem}: directory {:?} is listed as its own child",
                this.name
            );
        }
    }
}
