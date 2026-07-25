//! Fuzzes the hand-rolled `quick-xml` parsers for OBS API responses — the
//! `withfullhistory=1` request document, the request `<collection>`, the
//! `group?login` directory, and the `MAINT:RejectReason` attribute envelope.
//! This is network-controlled input (a compromised or MITM'd
//! `ssl_verify=false` response reaches these parsers verbatim), and it is the
//! surface the DTD/XXE pre-parse guard protects.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_datasources::obs::models::{
    parse_group_directory, parse_reject_reason_values, parse_request, parse_request_collection,
};

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_request(xml);
    let _ = parse_request_collection(xml);
    let _ = parse_group_directory(xml);
    let _ = parse_reject_reason_values(xml);
});
