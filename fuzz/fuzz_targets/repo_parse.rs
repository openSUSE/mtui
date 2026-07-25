//! Fuzzes the template repository/product metadata parsers in
//! `mtui-testreport`. The product strings and repository URL come from the
//! RRID template checkout — externally-sourced metadata.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_testreport::{gitrepoparse, parse_product, slrepoparse};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_product(text);
    // Split the input: first line is the repository URL, the rest is the
    // product-string list the `*repoparse` helpers walk.
    let (repo, rest) = text.split_once('\n').unwrap_or((text, ""));
    let products: Vec<String> = rest.lines().map(str::to_owned).collect();
    let _ = slrepoparse(repo, &products);
    let _ = gitrepoparse(repo, &products);
});
