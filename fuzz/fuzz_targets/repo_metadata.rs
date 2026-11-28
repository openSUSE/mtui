//! Fuzzes the rpm-md parsers behind `show_update_repos --check`: the
//! `repodata/repomd.xml` reader and the decompressing primary-metadata reader.
//! Both read whatever an update repository's server sends, so a hostile or
//! broken server reaches them verbatim (gzip, zstd or plain XML).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_datasources::repo_check::fuzzing::{parse_primary, parse_repomd};

fuzz_target!(|data: &[u8]| {
    let _ = parse_repomd(data);
    let listed = [("foo".to_owned(), "1.2-3.1.x86_64".to_owned())];
    let _ = parse_primary(data, &listed);
});
