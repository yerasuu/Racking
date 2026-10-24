//! Embeds the target triple so `rack update` asks the origin for the binary
//! built for the same platform as itself.

fn main() {
    // Cargo always sets TARGET for build scripts; an empty value only makes `rack update` find no release.
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo::rustc-env=RACK_TARGET={target}");
    println!("cargo::rerun-if-changed=build.rs");
}
