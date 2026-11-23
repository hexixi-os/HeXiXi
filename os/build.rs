use std::{env, path::PathBuf};

const RUNNER_PATH: &str = "../user/target/riscv64gc-unknown-none-elf/release/oscomp_runner";

fn main() {
    if env::var_os("CARGO_FEATURE_OSCOMP_RUNNER").is_some() {
        let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        println!("cargo:rerun-if-changed={}", RUNNER_PATH);
        println!(
            "cargo:rustc-env=OSCOMP_RUNNER_ELF={}",
            manifest.join(RUNNER_PATH).display()
        );
    }
}
