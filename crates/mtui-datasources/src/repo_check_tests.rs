//! Parser and bound tests for `repo_check`, plus the wire tests that need
//! limits other than the defaults.

use std::io::Write;
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::VerifyPolicy;

const PRIMARY: &str = include_str!("../tests/fixtures/rpm_md/primary.xml");
const REPOMD: &str = include_str!("../tests/fixtures/rpm_md/repomd.xml");

fn listed(pairs: &[(&str, &str)]) -> Listed {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

fn primary(body: &[u8], cap: usize, max_event: usize) -> Result<PrimaryScan, PrimaryError> {
    parse_primary(body, &Listed::new(), cap, max_event)
}

const BIG: usize = 1 << 20;

#[test]
fn repomd_yields_the_primary_href() {
    assert_eq!(
        parse_repomd(REPOMD.as_bytes()).as_deref(),
        Ok("repodata/00-primary.xml.gz")
    );
}

#[test]
fn repomd_refuses_a_dtd() {
    let xml = br#"<!DOCTYPE repomd [<!ENTITY x "y">]><repomd>
        <data type="primary"><location href="repodata/p.xml"/></data></repomd>"#;
    assert_eq!(parse_repomd(xml), Err(RepomdError::Unreadable));
}

#[test]
fn repomd_refuses_a_second_root() {
    let xml = br#"<repomd><data type="primary"><location href="a"/></data></repomd><repomd/>"#;
    assert_eq!(parse_repomd(xml), Err(RepomdError::Unreadable));
}

#[test]
fn primary_refuses_a_dtd() {
    let xml = br#"<!DOCTYPE metadata><metadata packages="0"></metadata>"#;
    assert_eq!(primary(xml, BIG, BIG), Err(PrimaryError::Unreadable));
}

#[test]
fn primary_records_only_listed_pairs_and_ignores_nested_names() {
    let xml = br#"<metadata xmlns:rpm="r" packages="2"><package type="rpm"><name>a</name>
        <arch>noarch</arch><version epoch="0" ver="1" rel="2"/>
        <format><rpm:provides><rpm:entry name="b" ver="9" rel="9"/></rpm:provides>
        <x><version ver="9" rel="9"></version></x></format>
        </package><package type="rpm"><name>c</name><arch>noarch</arch>
        <version epoch="0" ver="1" rel="1"/></package></metadata>"#;
    let want = listed(&[("a", "1-2.noarch"), ("b", "9-9.noarch")]);
    let scan = parse_primary(xml, &want, BIG, BIG).unwrap();
    assert_eq!(scan.count, 2);
    assert_eq!(scan.present, listed(&[("a", "1-2.noarch")]));
}

#[test]
fn deep_nesting_in_the_primary_is_unreadable() {
    let n = 10_000;
    let xml = format!(
        r#"<metadata packages="0">{}{}</metadata>"#,
        "<a>".repeat(n),
        "</a>".repeat(n)
    );
    assert_eq!(
        primary(xml.as_bytes(), BIG, BIG),
        Err(PrimaryError::Unreadable)
    );
}

#[test]
fn deep_nesting_in_repomd_is_unreadable() {
    let n = 10_000;
    let xml = format!(
        r#"<repomd><data type="primary"><location href="repodata/p.xml"/></data>{}{}</repomd>"#,
        "<a>".repeat(n),
        "</a>".repeat(n)
    );
    assert_eq!(parse_repomd(xml.as_bytes()), Err(RepomdError::Unreadable));
}

#[test]
fn cdata_or_a_reference_in_a_package_field_is_unreadable() {
    for name in ["<![CDATA[foo]]>", "f&#111;o", "foo&amp;"] {
        let xml = format!(
            r#"<metadata packages="1"><package><name>{name}</name><arch>noarch</arch><version ver="1" rel="1"/></package></metadata>"#
        );
        assert_eq!(
            primary(xml.as_bytes(), BIG, BIG),
            Err(PrimaryError::Unreadable),
            "{name}"
        );
    }
}

#[test]
fn a_zstd_primary_of_several_frames_is_read_whole() {
    let cut = PRIMARY.find("<package ").unwrap() + 10;
    let mut body = Vec::new();
    for part in [&PRIMARY[..cut], &PRIMARY[cut..]] {
        body.extend(ruzstd::encoding::compress_to_vec(
            part.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ));
    }
    assert_eq!(primary(&body, BIG, BIG).map(|s| s.count), Ok(3));
}

/// One raw, last-block zstd frame holding `content`, its window set by the
/// frame-header window descriptor byte.
fn zstd_raw_frame(window_descriptor: u8, content: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, window_descriptor];
    let block = (u32::try_from(content.len()).unwrap() << 3) | 1;
    frame.extend(&block.to_le_bytes()[..3]);
    frame.extend(content);
    frame
}

#[test]
fn markup_inside_a_package_field_is_unreadable() {
    let want = listed(&[("foobar", "1-1.noarch")]);
    for name in ["foo<b></b>bar", "foo<b/>bar"] {
        let xml = format!(
            r#"<metadata packages="1"><package><name>{name}</name><arch>noarch</arch><version ver="1" rel="1"/></package></metadata>"#
        );
        assert_eq!(
            parse_primary(xml.as_bytes(), &want, BIG, BIG),
            Err(PrimaryError::Unreadable),
            "{name}"
        );
    }
}

#[test]
fn a_zstd_frame_declaring_a_window_over_the_cap_is_unreadable() {
    let content = br#"<metadata packages="0"/>"#;
    // 0x78: exactly the 32 MiB cap; the same frame is read.
    assert_eq!(
        primary(&zstd_raw_frame(0x78, content), BIG, BIG).map(|s| s.count),
        Ok(0)
    );
    // 0x79: a 36 MiB window, just over the cap.
    assert_eq!(
        primary(&zstd_raw_frame(0x79, content), BIG, BIG),
        Err(PrimaryError::Unreadable)
    );
}

#[test]
fn a_zstd_frame_whose_checksum_does_not_match_is_unreadable() {
    let good = ruzstd::encoding::compress_to_vec(
        PRIMARY.as_bytes(),
        ruzstd::encoding::CompressionLevel::Uncompressed,
    );
    assert_eq!(primary(&good, BIG, BIG).map(|s| s.count), Ok(3));
    // Stored raw, so the flipped byte still decodes and parses: only the
    // frame checksum can tell.
    let at = good
        .windows(b"placeholder".len())
        .position(|w| w == b"placeholder")
        .unwrap();
    let mut bad = good;
    bad[at] = b'P';
    assert_eq!(primary(&bad, BIG, BIG), Err(PrimaryError::Unreadable));
}

#[test]
fn primary_self_closed_root_is_empty() {
    let scan = primary(br#"<metadata packages="0"/>"#, BIG, BIG).unwrap();
    assert_eq!(scan.count, 0);
}

#[test]
fn primary_over_the_cap_is_too_large() {
    let xml = br#"<metadata packages="0"></metadata>"#;
    assert_eq!(primary(xml, 8, BIG), Err(PrimaryError::TooLarge(8)));
}

#[test]
fn a_gzip_bomb_stops_at_the_decompressed_cap() {
    let mut xml = br#"<metadata packages="0">"#.to_vec();
    xml.extend(std::iter::repeat_n(b' ', 4 * BIG));
    xml.extend(b"</metadata>");
    let bomb = gzip(&xml);
    assert!(bomb.len() < 64 * 1024, "compressed {} bytes", bomb.len());
    assert_eq!(primary(&bomb, BIG, BIG), Err(PrimaryError::TooLarge(BIG)));
}

#[test]
fn one_huge_text_node_is_refused_before_it_is_buffered() {
    let mut xml = br#"<metadata packages="1"><package><name>a</name><description>"#.to_vec();
    xml.extend(std::iter::repeat_n(b'x', 2 * BIG));
    xml.extend(b"</description></package></metadata>");
    let bomb = gzip(&xml);
    assert_eq!(
        primary(&bomb, 16 * BIG, 64 * 1024),
        Err(PrimaryError::EventTooLarge(64 * 1024))
    );
}

#[test]
fn a_document_of_many_small_events_passes_the_per_event_bound() {
    let mut xml = br#"<metadata packages="2000">"#.to_vec();
    for _ in 0..2000 {
        xml.extend(
            br#"<package><name>a</name><arch>noarch</arch><version ver="1" rel="1"/></package>"#,
        );
    }
    xml.extend(b"</metadata>");
    assert!(xml.len() > 4 * 32 * 1024);
    assert_eq!(primary(&xml, BIG, 32 * 1024).map(|s| s.count), Ok(2000));
}

#[test]
fn a_package_name_over_the_field_limit_is_unreadable() {
    let name = "a".repeat(MAX_FIELD + 1);
    let xml =
        format!(r#"<metadata packages="1"><package><name>{name}</name></package></metadata>"#);
    assert_eq!(
        primary(xml.as_bytes(), BIG, BIG),
        Err(PrimaryError::Unreadable)
    );
}

#[test]
fn a_version_over_the_field_limit_is_unreadable() {
    let ver = "1".repeat(MAX_FIELD + 1);
    let xml = format!(
        r#"<metadata packages="1"><package><name>a</name><version ver="{ver}" rel="1"/></package></metadata>"#
    );
    assert_eq!(
        primary(xml.as_bytes(), BIG, BIG),
        Err(PrimaryError::Unreadable)
    );
}

#[test]
fn the_fixture_primary_is_read() {
    let scan = parse_primary(
        PRIMARY.as_bytes(),
        &listed(&[("foo-doc", "1.2-3.1.noarch")]),
        BIG,
        BIG,
    )
    .unwrap();
    assert_eq!(scan.count, 3);
    assert_eq!(scan.present.len(), 1);
}

#[test]
fn within_refuses_paths_and_origins_outside_the_base() {
    let base = repository_base("http://u:p@h:80/r/x").unwrap();
    assert_eq!(base.as_str(), "http://u:p@h/r/x/");
    let ok = within(&base, &base.join("repodata/p.xml").unwrap()).unwrap();
    assert_eq!(ok.as_str(), "http://u:p@h/r/x/repodata/p.xml");
    let absolute = within(&base, &Url::parse("http://h/r/x/repodata/p.xml").unwrap()).unwrap();
    assert_eq!(absolute.as_str(), "http://u:p@h/r/x/repodata/p.xml");
    for href in [
        "../y/repodata/p.xml",
        "/repodata/p.xml",
        "http://other/r/x/repodata/p.xml",
        "//other/r/x/p.xml",
        "https://h/r/x/p.xml",
        "http://h:81/r/x/p.xml",
        "http://v:q@h/r/x/p.xml",
        "http://h/r/xevil/p.xml",
        "../xevil/p.xml",
        "repodata/..%2fp.xml",
        "",
    ] {
        let url = base.join(href).unwrap();
        assert!(within(&base, &url).is_none(), "{href} must be refused");
    }
}

#[test]
fn repository_base_refuses_non_http() {
    assert!(repository_base("ftp://h/r").is_none());
    assert!(repository_base("not a url").is_none());
    assert!(repository_base("file:///r").is_none());
}

fn client() -> HttpClient {
    HttpClient::new(VerifyPolicy::Default(true)).expect("client builds")
}

async fn serve(server: &MockServer, at: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_slow_repository_is_no_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(REPOMD)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let limits = CheckLimits {
        request_timeout: Duration::from_millis(200),
        ..CheckLimits::default()
    };
    let got = check_with(&client(), &server.uri(), &Listed::new(), &limits).await;
    assert_eq!(
        got,
        RepoCheck::NoAnswer(NoAnswer::Timeout(Document::Repomd))
    );
    assert_eq!(
        got.to_string(),
        "repodata/repomd.xml did not answer in time"
    );
}

#[tokio::test]
async fn a_repomd_over_its_wire_cap_is_no_answer() {
    let server = MockServer::start().await;
    serve(&server, "/repodata/repomd.xml", REPOMD.as_bytes().to_vec()).await;
    let limits = CheckLimits {
        max_repomd: 64,
        ..CheckLimits::default()
    };
    let got = check_with(&client(), &server.uri(), &Listed::new(), &limits).await;
    assert_eq!(
        got,
        RepoCheck::NoAnswer(NoAnswer::TooLarge(Document::Repomd, 64))
    );
}

#[tokio::test]
async fn a_primary_over_its_wire_cap_is_no_answer() {
    let server = MockServer::start().await;
    serve(&server, "/repodata/repomd.xml", REPOMD.as_bytes().to_vec()).await;
    serve(
        &server,
        "/repodata/00-primary.xml.gz",
        gzip(PRIMARY.as_bytes()),
    )
    .await;
    let limits = CheckLimits {
        max_primary: 64,
        ..CheckLimits::default()
    };
    let got = check_with(&client(), &server.uri(), &Listed::new(), &limits).await;
    assert_eq!(
        got,
        RepoCheck::NoAnswer(NoAnswer::TooLarge(Document::Primary, 64))
    );
}

#[tokio::test]
async fn a_served_bomb_under_the_wire_cap_is_no_answer() {
    let mut xml = br#"<metadata packages="0">"#.to_vec();
    xml.extend(std::iter::repeat_n(b' ', 4 * BIG));
    xml.extend(b"</metadata>");
    let server = MockServer::start().await;
    serve(&server, "/repodata/repomd.xml", REPOMD.as_bytes().to_vec()).await;
    serve(&server, "/repodata/00-primary.xml.gz", gzip(&xml)).await;
    let limits = CheckLimits {
        max_unpacked: BIG,
        ..CheckLimits::default()
    };
    let got = check_with(&client(), &server.uri(), &Listed::new(), &limits).await;
    assert_eq!(
        got,
        RepoCheck::NoAnswer(NoAnswer::Primary(PrimaryError::TooLarge(BIG)))
    );
}
