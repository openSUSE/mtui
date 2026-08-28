//! Guards against `output filename collision` in `cargo doc --workspace`.
//!
//! Cargo derives each documented target's output directory from its name with
//! `-` replaced by `_`; a lib and a bin that collide there race for the same
//! `target/doc/<name>/index.html` and Cargo only warns, never errors. This
//! test asserts the collision offline, from `cargo metadata`, instead of
//! parsing rustdoc's build output.

use std::collections::HashMap;
use std::process::Command;

#[test]
fn no_doc_output_target_collisions() {
    let manifest_path = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            &manifest_path,
        ])
        .output()
        .expect("cargo metadata should run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata should emit valid JSON");

    let mut by_doc_name: HashMap<String, String> = HashMap::new();
    for package in metadata["packages"]
        .as_array()
        .expect("packages should be an array")
    {
        let package_name = package["name"].as_str().expect("package name");
        let targets = package["targets"]
            .as_array()
            .expect("targets should be an array");

        // Cargo silently skips documenting a bin whose name matches a lib in
        // the same package (the common `lib.rs` + `main.rs` pattern) instead
        // of warning — that self-pairing is not the collision this test
        // guards against, so it must not be flagged as one.
        let lib_names: Vec<&str> = targets
            .iter()
            .filter(|t| is_lib_ish(t))
            .map(|t| t["name"].as_str().expect("target name"))
            .collect();

        for target in targets {
            if target["doc"].as_bool() != Some(true) {
                continue;
            }
            let target_name = target["name"].as_str().expect("target name");
            let is_lib = is_lib_ish(target);
            let is_bin = target["kind"]
                .as_array()
                .expect("target kind should be an array")
                .iter()
                .any(|k| k.as_str() == Some("bin"));
            if !is_lib && !is_bin {
                continue;
            }
            if is_bin && lib_names.contains(&target_name) {
                continue;
            }

            let doc_dir = target_name.replace('-', "_");
            let site = format!("{package_name}:{target_name}");
            if let Some(existing) = by_doc_name.insert(doc_dir.clone(), site.clone()) {
                panic!(
                    "doc output collision at target/doc/{doc_dir}/: {existing} and {site} both write there"
                );
            }
        }
    }
}

fn is_lib_ish(target: &serde_json::Value) -> bool {
    target["kind"]
        .as_array()
        .expect("target kind should be an array")
        .iter()
        .any(|k| matches!(k.as_str(), Some("lib" | "rlib" | "dylib" | "proc-macro")))
}
