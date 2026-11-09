//! Records which PSCat revision this binary was built against, read from
//! `Cargo.lock` so the reported version can never drift from the real one.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = fs::read_to_string("Cargo.lock").unwrap_or_default();
    let mut version = String::from("unknown");
    let mut source = String::from("unknown");
    let mut in_pscat = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_pscat = false;
        } else if line == "name = \"pscat\"" {
            in_pscat = true;
        } else if in_pscat {
            if let Some(v) = line.strip_prefix("version = ") {
                version = v.trim_matches('"').to_string();
            } else if let Some(s) = line.strip_prefix("source = ") {
                source = s.trim_matches('"').to_string();
            }
        }
    }
    println!("cargo:rustc-env=PSCAT_VERSION={version}");
    println!("cargo:rustc-env=PSCAT_SOURCE={source}");
}
