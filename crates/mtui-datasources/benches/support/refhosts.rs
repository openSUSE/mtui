//! Synthetic refhosts fixture shared by the criterion benches and the
//! integration tests that pin the workload the benches claim to measure.

use mtui_datasources::Attributes;

/// Builds a legacy location-grouped refhosts.yml string with `n` hosts spread
/// across a few location groups (so the flatten/dedup path is exercised).
pub fn synthetic_refhosts_yaml(n: usize) -> String {
    let mut s = String::from("group_a:\n");
    for i in 0..n {
        if i == n / 2 {
            s.push_str("group_b:\n");
        }
        let major = 12 + (i % 4);
        let minor = i % 6;
        s.push_str(&format!(
            "  - name: host-{i:05}\n    arch: {arch}\n    product:\n      name: sles\n      version:\n        major: {major}\n        minor: {minor}\n",
            arch = if i % 2 == 0 { "x86_64" } else { "aarch64" },
        ));
    }
    s
}

/// One `refhosts/search` workload: the queries and the exact names they must
/// return, derived from the generator's arithmetic rather than from `search`.
pub struct SearchCase {
    pub label: &'static str,
    pub attrs: Vec<Attributes>,
    pub expected: Vec<String>,
}

pub fn host_name(i: usize) -> String {
    format!("host-{i:05}")
}

/// SLES 15 SP5 on x86_64: the generator gives every major-15 host an odd index,
/// hence aarch64, so this matches nothing.
pub fn all_miss_query() -> Vec<Attributes> {
    Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]")
}

/// SLES 15 SP5 on aarch64: matches exactly the hosts with `i % 12 == 11`.
pub fn sparse_hit_query() -> Vec<Attributes> {
    Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[aarch64]")
}

pub fn sparse_hit_names(n: usize) -> Vec<String> {
    (0..n).filter(|i| i % 12 == 11).map(host_name).collect()
}

pub fn search_cases(n: usize) -> Vec<SearchCase> {
    let sparse = sparse_hit_query();
    assert_eq!(sparse.len(), 1, "one-arch query yields one attribute");
    let miss = all_miss_query();
    assert_eq!(miss.len(), 1, "one-arch query yields one attribute");

    let sparse_names = sparse_hit_names(n);
    vec![
        SearchCase {
            label: "all_miss",
            attrs: miss.clone(),
            expected: Vec::new(),
        },
        SearchCase {
            label: "sparse_hit",
            attrs: sparse.clone(),
            expected: sparse_names.clone(),
        },
        SearchCase {
            label: "mixed_hit_miss",
            attrs: sparse.iter().chain(&miss).cloned().collect(),
            expected: sparse_names.clone(),
        },
        SearchCase {
            label: "dense_hit",
            attrs: vec![Attributes::default()],
            expected: (0..n).map(host_name).collect(),
        },
        SearchCase {
            label: "duplicate_query",
            attrs: sparse.iter().chain(&sparse).cloned().collect(),
            expected: sparse_names.iter().chain(&sparse_names).cloned().collect(),
        },
    ]
}
