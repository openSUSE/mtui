//! Fuzzes the parser that reads the downgrade probe's output back from a
//! managed host. It keeps each package's highest version by RPM ordering; if
//! that ordering is not a total order, a lower version can be kept, and the
//! target asserts that none is.
#![no_main]

use std::cmp::Ordering;

use libfuzzer_sys::fuzz_target;
use mtui_testreport::fuzzing::parse_downgrade_versions;
use mtui_types::rpmver::RPMVersion;

fuzz_target!(|data: &[u8]| {
    // The SSH capture decodes host output the same way.
    let output = String::from_utf8_lossy(data);
    let kept = parse_downgrade_versions(&output);

    for (name, version) in output.lines().filter_map(|line| line.split_once(" = ")) {
        let Ok(candidate) = RPMVersion::parse(version) else {
            continue;
        };
        let highest = RPMVersion::parse(&kept[name])
            .expect("a parsable version is kept over an unparsable one");
        assert_ne!(
            highest.cmp(&candidate),
            Ordering::Less,
            "{name}: kept {:?} below {version:?}",
            kept[name]
        );
    }
});
