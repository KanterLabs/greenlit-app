//! Embed one validated source identity in the compiled CLI version string.

use std::env::VarError;

const INPUT: &str = "GREENLIT_BUILD_COMMIT";
const UNVERIFIED: &str = "unverified";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(litci_test_boundaries)");
    println!("cargo:rerun-if-env-changed={INPUT}");
    let commit = match std::env::var(INPUT) {
        Ok(value)
            if value.len() == 40
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            value
        }
        Ok(_) => {
            eprintln!("{INPUT} must be exactly 40 lowercase hexadecimal characters when supplied");
            std::process::exit(1);
        }
        Err(VarError::NotPresent) => UNVERIFIED.to_string(),
        Err(VarError::NotUnicode(_)) => {
            eprintln!("{INPUT} must be UTF-8 lowercase hexadecimal");
            std::process::exit(1);
        }
    };
    println!("cargo:rustc-env={INPUT}={commit}");
}
