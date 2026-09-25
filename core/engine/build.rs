//! Builds the actual opcode-handler stencil library for the baseline JIT.
//!
//! The canonical LLVM module is produced from this exact crate with `boa_jit_template_export`.
//! This build script lowers that module into the native archive used for stencil extraction and
//! also uses it to generate the typed resolver. Using one module for both artifacts is important:
//! private Rust symbol hashes are not stable across independent compilations.

#[path = "build/archive_closure.rs"]
mod archive_closure;
#[path = "build/llvm_resolver.rs"]
mod llvm_resolver;
#[path = "build/llvm_stencils.rs"]
mod llvm_stencils;
#[path = "build/template_archive.rs"]
mod template_archive;

use std::{env, fs, path::PathBuf};

const OPCODE_COUNT: usize = 256;
const HANDLER_TABLE: &str = "BOA_JIT_TEMPLATE_HANDLERS";
const CONFIG_SYMBOL: &str = "BOA_JIT_TEMPLATE_CONFIG";

fn jit_config() -> (u64, String) {
    let mut entries: Vec<String> = env::vars()
        .filter_map(|(name, _)| name.strip_prefix("CARGO_FEATURE_").map(str::to_owned))
        .collect();
    entries.push("JIT_ABI=chain-v4-specialized".into());
    for name in [
        "CARGO_CFG_TARGET_ARCH",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ENV",
        "CARGO_CFG_TARGET_FAMILY",
        "CARGO_CFG_TARGET_ENDIAN",
        "CARGO_CFG_TARGET_POINTER_WIDTH",
        "CARGO_CFG_PANIC",
    ] {
        entries.push(format!("{name}={}", env::var(name).unwrap_or_default()));
    }
    entries.sort();
    let description = entries.join(",");
    let mut fingerprint = 0xcbf29ce484222325_u64;
    for byte in description.bytes() {
        fingerprint ^= u64::from(byte);
        fingerprint = fingerprint.wrapping_mul(0x100000001b3);
    }
    (fingerprint, description)
}

fn main() {
    println!("cargo::rerun-if-changed=src/vm/opcode/mod.rs");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(boa_jit_stencils)");
    println!("cargo::rerun-if-env-changed=BOA_JIT_BUILD_TEMPLATE");
    println!("cargo::rerun-if-env-changed=BOA_JIT_TEMPLATE_LLVM_IR");
    println!("cargo::rerun-if-env-changed=LLVM_LLC");
    println!("cargo::rerun-if-env-changed=LLVM_AR");
    println!("cargo::rerun-if-env-changed=BOA_JIT_REUSE_GENERATED");

    if env::var_os("CARGO_FEATURE_JIT").is_none() {
        return;
    }
    let (config_fingerprint, config_description) = jit_config();
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
    fs::write(
        out.join("jit_config_generated.rs"),
        format!(
            "#[allow(missing_docs)]\n#[unsafe(no_mangle)]\n#[used]\npub static {CONFIG_SYMBOL}: u64 = {config_fingerprint:#018x};\n"
        ),
    )
    .expect("cannot write JIT configuration marker");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo must set target arch");
    let family = env::var("CARGO_CFG_TARGET_FAMILY").expect("Cargo must set target family");
    if arch != "x86_64" || family != "unix" {
        println!("cargo::warning=the baseline JIT is currently disabled outside x86-64 Unix");
        return;
    }
    if env::var_os("BOA_JIT_BUILD_TEMPLATE").is_some() {
        return;
    }

    let llvm_ir = env::var_os("BOA_JIT_TEMPLATE_LLVM_IR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "jit requires BOA_JIT_TEMPLATE_LLVM_IR pointing to the canonical boa_engine LLVM module; see core/engine/src/jit/README.md"
            )
        });
    println!("cargo::rerun-if-changed={}", llvm_ir.display());
    let mut generation_key = config_fingerprint;
    for bytes in [
        fs::read(&llvm_ir).expect("cannot read canonical LLVM module"),
        include_bytes!("build.rs").to_vec(),
        include_bytes!("build/llvm_stencils.rs").to_vec(),
        include_bytes!("build/llvm_resolver.rs").to_vec(),
        include_bytes!("build/archive_closure.rs").to_vec(),
        include_bytes!("build/template_archive.rs").to_vec(),
    ] {
        for byte in bytes {
            generation_key = (generation_key ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    let generation_key = format!("{generation_key:016x}");
    let reusable = [
        "jit_stencils.bin",
        "jit_internal_closure.bin",
        "jit_stencils_generated.rs",
        "libboa_jit_resolver.a",
    ]
    .iter()
    .all(|name| out.join(name).is_file())
        && fs::read_to_string(out.join("jit_generation_key"))
            .ok()
            .as_deref()
            == Some(&generation_key);
    if env::var_os("BOA_JIT_REUSE_GENERATED").is_some() && reusable {
        println!("cargo::warning=reusing existing JIT stencil and resolver artifacts");
        println!("cargo::rustc-link-search=native={}", out.display());
        println!("cargo::rustc-link-lib=static:-bundle=boa_jit_resolver");
        println!("cargo::rustc-cfg=boa_jit_stencils");
        return;
    }

    let shallow_ir = out.join("boa_jit_shallow.bc");
    let holes = llvm_stencils::lower(&llvm_ir, &shallow_ir)
        .unwrap_or_else(|error| panic!("cannot create shallow JIT module: {error}"));
    let archive_path = llvm_resolver::emit_template_archive(&shallow_ir, &out)
        .unwrap_or_else(|error| panic!("cannot lower canonical JIT LLVM module: {error}"));
    let archive_data = fs::read(archive_path)
        .unwrap_or_else(|error| panic!("cannot read generated JIT template archive: {error}"));
    let template_fingerprint = template_archive::read_u64_symbol(&archive_data, CONFIG_SYMBOL)
        .unwrap_or_else(|error| panic!("invalid JIT template configuration marker: {error}"));
    assert_eq!(
        template_fingerprint, config_fingerprint,
        "JIT template/runtime configuration mismatch (template {template_fingerprint:#018x}, runtime {config_fingerprint:#018x}). Rebuild the template module with the exact boa_engine features used by this target. Runtime configuration: {config_description}"
    );

    let generated = archive_closure::generate(
        &archive_data,
        HANDLER_TABLE,
        OPCODE_COUNT,
        &shallow_ir,
        &llvm_ir,
        &holes,
    )
    .unwrap_or_else(|error| panic!("invalid archive closure: {error}"));
    println!(
        "cargo::warning=JIT archive generator: {} members, {} closure sections, {} bytes, {} relocations, {} externals, {} supported opcodes",
        generated.members,
        generated.sections,
        generated.bytes,
        generated.relocations,
        generated.externals,
        generated.supported
    );
    fs::write(out.join("jit_generation_key"), generation_key)
        .expect("cannot write JIT generation key");
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static:-bundle=boa_jit_resolver");
    println!("cargo::rustc-cfg=boa_jit_stencils");
}
