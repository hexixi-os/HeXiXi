use std::{env, path::PathBuf};

fn main() {
    if env::var_os("CARGO_FEATURE_OSCOMP_RUNNER").is_some() {
        let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let target = env::var("TARGET").unwrap();
        let runner_path = format!("../user/target/{target}/release/oscomp_runner");
        let runner = manifest.join(runner_path);
        println!("cargo:rerun-if-changed={}", runner.display());
        println!(
            "cargo:rustc-env=OSCOMP_RUNNER_ELF={}",
            runner.display()
        );
    }
}
