//! Loads every `.lisp` file in `crates/newgc-test-lisp/scripts/` and
//! runs it through the evaluator. Each script self-verifies via
//! `assert` / `check-equal` / `error`. Any panic / Err from the
//! evaluator fails the corresponding test case.
//!
//! This lets workload authors write new tests by dropping a `.lisp`
//! file into `scripts/` — no Rust edit needed.
//!
//! Each script gets its own test name derived from the filename, so
//! `cargo test --test run_scripts script_03_tree_walk` runs just one.

use std::fs;
use std::path::PathBuf;

use newgc_test_lisp::Interpreter;

fn scripts_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/newgc-test-lisp/
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("scripts");
    p
}

fn run_script_file(path: &std::path::Path) {
    let src = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let mut interp = Interpreter::new(32 * 64 * 1024);
    interp.set_minor_threshold(200);
    interp.set_majors_every(5);
    if let Err(e) = interp.run_source(&src) {
        panic!(
            "script {path:?} failed: {}\nstats: {:?}",
            e.msg(),
            interp.stats
        );
    }
    eprintln!(
        "script {} ok: {} alloc, {} minor + {} major GCs, {} fn-calls",
        path.file_name().unwrap().to_string_lossy(),
        interp.stats.allocations,
        interp.stats.minor_gcs,
        interp.stats.major_gcs,
        interp.stats.function_calls,
    );
}

#[test]
fn script_01_arithmetic() {
    run_script_file(&scripts_dir().join("01-arithmetic.lisp"));
}

#[test]
fn script_02_list_survival() {
    run_script_file(&scripts_dir().join("02-list-survival.lisp"));
}

#[test]
fn script_03_tree_walk() {
    run_script_file(&scripts_dir().join("03-tree-walk.lisp"));
}

#[test]
fn script_04_mutation_survives() {
    run_script_file(&scripts_dir().join("04-mutation-survives.lisp"));
}

#[test]
fn script_05_stochastic_churn() {
    run_script_file(&scripts_dir().join("05-stochastic-churn.lisp"));
}

#[test]
fn script_06_shared_structure() {
    run_script_file(&scripts_dir().join("06-shared-structure.lisp"));
}

#[test]
fn script_07_gc_stats() {
    run_script_file(&scripts_dir().join("07-gc-stats.lisp"));
}

/// Auto-discovery: iterate every `.lisp` file under `scripts/` and
/// fail if any unrecognized scripts exist. This catches "I added a
/// file but forgot to add a #[test]" — and serves as an inventory.
#[test]
fn no_orphan_scripts() {
    let dir = scripts_dir();
    if !dir.exists() {
        return;
    }
    let mut found: Vec<String> = fs::read_dir(&dir)
        .expect("scripts/ readable")
        .filter_map(|e| {
            let e = e.ok()?;
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("lisp") {
                p.file_name().map(|s| s.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    found.sort();

    let known = [
        "01-arithmetic.lisp",
        "02-list-survival.lisp",
        "03-tree-walk.lisp",
        "04-mutation-survives.lisp",
        "05-stochastic-churn.lisp",
        "06-shared-structure.lisp",
        "07-gc-stats.lisp",
    ];
    let known_set: std::collections::HashSet<&str> =
        known.iter().copied().collect();
    let unknown: Vec<&String> = found
        .iter()
        .filter(|n| !known_set.contains(n.as_str()))
        .collect();
    assert!(
        unknown.is_empty(),
        "scripts/ contains files without a #[test] entry: {:?}\n\
         Add a test function to run_scripts.rs that calls run_script_file().",
        unknown
    );
}
