//! Fuzzes the `/etc/products.d/*.prod` and `/etc/os-release` parsers with
//! host-controlled file bytes (SFTP-fetched from managed reference hosts).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_hosts::target::parsers::product::{parse_os_release, parse_product};

fuzz_target!(|data: &[u8]| {
    let _ = parse_product(data);
    let _ = parse_os_release(data);
});
