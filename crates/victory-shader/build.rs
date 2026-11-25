mod fingerprint_inputs;

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::process::Command;

use fingerprint_inputs::{INPUTS, field, source_inputs};

fn main() {
    for input in INPUTS {
        println!("cargo:rerun-if-changed={input}");
    }
    let (mut hash, switches) = source_inputs(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("cannot fingerprint the shader translator");
    let rustc = Command::new(env::var_os("RUSTC").expect("Cargo sets RUSTC"))
        .arg("-vV")
        .output()
        .expect("query shader compiler version");
    assert!(rustc.status.success(), "rustc -vV failed");
    field(&mut hash, &rustc.stdout);
    // These are Cargo's effective settings for victory-shader itself.
    let settings: BTreeMap<_, _> = env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_FEATURE_")
                || name.starts_with("CARGO_CFG_")
                || matches!(
                    name.as_ref(),
                    "TARGET" | "PROFILE" | "OPT_LEVEL" | "DEBUG" | "CARGO_ENCODED_RUSTFLAGS"
                )
        })
        .collect();
    for (name, value) in settings {
        field(&mut hash, name.as_encoded_bytes());
        field(&mut hash, value.as_encoded_bytes());
    }
    println!("cargo:rustc-env=TRANSLATOR_INPUTS_HASH={}", hash.finalize());
    println!(
        "cargo:rustc-env=TRANSLATOR_SWITCH_NAMES={}",
        switches.into_iter().collect::<Vec<_>>().join(",")
    );
}
