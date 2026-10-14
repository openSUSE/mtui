//! Pins the synthetic workload behind the `refhosts/search` benches, so a
//! benchmark cannot silently keep timing a query that matches nothing.

use mtui_datasources::{Attributes, Refhosts};
use mtui_types::{Version, VersionField, load_refhosts};

#[path = "../benches/support/refhosts.rs"]
mod support;

use support::{all_miss_query, host_name, search_cases, sparse_hit_names, sparse_hit_query};

const SIZES: [usize; 5] = [0, 1, 16, 128, 1024];

fn store(n: usize) -> Refhosts {
    let hosts = load_refhosts(&support::synthetic_refhosts_yaml(n)).expect("synthetic yaml parses");
    Refhosts::from_hosts(hosts)
}

#[test]
fn the_synthetic_fleet_has_the_documented_shape() {
    for n in SIZES {
        let rh = store(n);
        assert_eq!(rh.hosts().len(), n, "host count at n={n}");
        for (i, host) in rh.hosts().iter().enumerate() {
            assert_eq!(host.name, host_name(i), "order at n={n}");
            assert_eq!(host.arch, if i % 2 == 0 { "x86_64" } else { "aarch64" });
            assert_eq!(host.product.name, "sles");
            assert_eq!(
                host.product.version,
                Some(Version::new(
                    (12 + i % 4) as u64,
                    Some(VersionField::Num((i % 6) as u64)),
                )),
                "version of {} at n={n}",
                host.name
            );
        }
    }
}

#[test]
fn the_fleet_is_split_into_two_location_groups_from_the_middle() {
    for n in SIZES {
        let yaml = support::synthetic_refhosts_yaml(n);
        assert_eq!(yaml.matches("group_a:").count(), 1);
        assert_eq!(yaml.matches("group_b:").count(), usize::from(n > 0));
        if n > 0 {
            let b = yaml.find("group_b:").unwrap();
            assert_eq!(
                yaml.find(&format!("name: {}\n", host_name(n / 2)))
                    .map(|p| p > b),
                Some(true),
                "group_b starts at host {} (n={n})",
                n / 2
            );
            if n > 1 {
                let before = yaml.find(&format!("name: {}\n", host_name(n / 2 - 1)));
                assert!(before.is_some_and(|p| p < b), "n={n}");
            }
        }
    }
}

#[test]
fn the_sparse_hit_counts_follow_the_generator() {
    let counts: Vec<usize> = [16, 128, 1024]
        .into_iter()
        .map(|n| sparse_hit_names(n).len())
        .collect();
    assert_eq!(counts, [1, 10, 85]);
    assert_eq!(sparse_hit_names(128).first().unwrap(), "host-00011");
    assert_eq!(sparse_hit_names(128).last().unwrap(), "host-00119");
}

#[test]
fn the_original_x86_64_query_matches_nothing() {
    for n in [16, 128, 1024] {
        assert!(store(n).search(&all_miss_query()).is_empty(), "n={n}");
    }
}

#[test]
fn the_sparse_query_hits_the_expected_hosts() {
    for n in [16, 128, 1024] {
        let got = store(n).search(&sparse_hit_query());
        assert!(!got.is_empty(), "no match at n={n}");
        assert_eq!(got, sparse_hit_names(n), "n={n}");
    }
}

#[test]
fn every_search_case_returns_exactly_its_expectation() {
    for n in SIZES {
        let rh = store(n);
        let cases = search_cases(n);
        let labels: Vec<_> = cases.iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            [
                "all_miss",
                "sparse_hit",
                "mixed_hit_miss",
                "dense_hit",
                "duplicate_query"
            ]
        );
        for case in cases {
            assert_eq!(
                rh.search(&case.attrs),
                case.expected,
                "{} at n={n}",
                case.label
            );
        }
    }
}

#[test]
fn case_sizes_match_the_documented_counts() {
    let sizes = |label: &str| -> Vec<usize> {
        [16, 128, 1024]
            .into_iter()
            .map(|n| {
                search_cases(n)
                    .into_iter()
                    .find(|c| c.label == label)
                    .unwrap()
                    .expected
                    .len()
            })
            .collect()
    };
    assert_eq!(sizes("all_miss"), [0, 0, 0]);
    assert_eq!(sizes("sparse_hit"), [1, 10, 85]);
    assert_eq!(sizes("mixed_hit_miss"), [1, 10, 85]);
    assert_eq!(sizes("dense_hit"), [16, 128, 1024]);
    assert_eq!(sizes("duplicate_query"), [2, 20, 170]);
}

#[test]
fn duplicate_queries_concatenate_per_query_instead_of_interleaving() {
    let rh = store(128);
    let sparse = sparse_hit_names(128);
    let doubled: Vec<String> = sparse.iter().chain(&sparse).cloned().collect();
    let attrs: Vec<Attributes> = sparse_hit_query()
        .into_iter()
        .chain(sparse_hit_query())
        .collect();
    let got = rh.search(&attrs);
    assert_eq!(got, doubled);
    let interleaved: Vec<String> = sparse.iter().flat_map(|n| [n.clone(), n.clone()]).collect();
    assert_ne!(
        got, interleaved,
        "128 hosts give ten sparse hits, so order is observable"
    );
}

#[test]
fn query_order_does_not_change_a_mixed_result() {
    let rh = store(128);
    let attrs: Vec<Attributes> = all_miss_query()
        .into_iter()
        .chain(sparse_hit_query())
        .collect();
    assert_eq!(rh.search(&attrs), sparse_hit_names(128));
}

#[test]
fn no_queries_match_nothing_but_one_unconstrained_query_matches_every_host() {
    for n in [0, 1, 16] {
        let rh = store(n);
        assert!(rh.search(&[]).is_empty());
        let all: Vec<String> = (0..n).map(host_name).collect();
        assert_eq!(rh.search(&[Attributes::default()]), all);
    }
}
