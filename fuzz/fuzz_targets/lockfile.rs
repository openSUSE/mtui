//! Fuzzes the remote lockfile line parser (`timestamp:user:pid[:comment]`).
//! The lockfile lives on the managed host, so its content is host-controlled.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_hosts::target::RemoteLock;

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    let _ = RemoteLock::from_lockfile(line);
});
