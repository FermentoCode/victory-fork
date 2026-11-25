#[path = "../fingerprint_inputs.rs"]
mod fingerprint_inputs;

use std::fs;

#[test]
fn compiler_supplies_a_stable_shader_source_key() {
    let key = victory_shader::emit::TRANSLATOR_INPUTS_HASH;
    assert_eq!(key.len(), 64);
    assert!(key.bytes().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn renderer_changes_share_a_key_but_shader_and_dependency_changes_do_not() {
    let root = tempfile::tempdir().unwrap();
    let shader = root.path().join("crates/victory-shader");
    fs::create_dir_all(shader.join("src/emit")).unwrap();
    for file in &fingerprint_inputs::INPUTS[1..] {
        fs::write(shader.join(file), b"fixture").unwrap();
    }
    let lower = shader.join("src/emit/lower.rs");
    fs::write(&lower, r#"env::var_os("VICTORY_F16_NEAREST")"#).unwrap();
    let key = || {
        fingerprint_inputs::source_inputs(&shader)
            .unwrap()
            .0
            .finalize()
    };
    let original = key();
    for file in [
        "apps/victory/src/main.rs",
        "crates/victory-gpu/src/diagnostics.rs",
    ] {
        let file = root.path().join(file);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, b"unrelated change").unwrap();
    }
    assert_eq!(key(), original);
    let switches = fingerprint_inputs::source_inputs(&shader).unwrap().1;
    assert!(switches.contains("VICTORY_F16_NEAREST"));
    fs::write(lower, "changed floating-point rounding").unwrap();
    let changed = key();
    assert_ne!(changed, original);
    fs::write(root.path().join("Cargo.lock"), b"new dependency checksum").unwrap();
    assert_ne!(key(), changed);
}

#[test]
fn translator_lists_local_behavior_switches_for_runtime_keys() {
    let names: Vec<_> = victory_shader::emit::TRANSLATOR_SWITCH_NAMES
        .split(',')
        .collect();
    for required in [
        "VICTORY_F16_NEAREST",
        "VICTORY_GROUP_SYNC",
        "VICTORY_SHADER_LOOP_LIMIT",
        "VICTORY_SHADER_OPT",
        "VICTORY_WAVE_WIDE",
        "VICTORY_WAVE_LOCAL",
        "VICTORY_WAVE_DEEP_SKIPS",
        "VICTORY_WAVE_OWN_BITS",
        "VICTORY_MESH_OUTPUT_STRICT",
        "VICTORY_STRICT_JOINS",
    ] {
        assert!(names.contains(&required), "{required} missing from the key");
    }
}
