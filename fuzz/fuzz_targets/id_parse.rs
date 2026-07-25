//! Fuzzes the small identifier grammars in `mtui-types`: RRID / UpdateID
//! (CLI + OBS request IDs), RPM version strings (`rpm -q` output from managed
//! hosts), package specs (shell-metacharacter rejection is security-relevant),
//! and repository URLs (template metadata).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_types::{PackageSpec, RPMVersion, RepoUrl, RequestReviewID, UpdateID};

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = RequestReviewID::parse(s);
    let _ = UpdateID::parse(s);
    let _ = RPMVersion::parse(s);
    let _ = PackageSpec::parse(s);
    let _ = RepoUrl::parse(s);
});
