//! Compiles the host target triple into the binary.
//!
//! Self-update selects its release asset by target triple. Reading the triple
//! from the compiler instead of guessing it at runtime means a node can never
//! download an executable built for a different architecture or libc.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TARGET");
    let target = std::env::var("TARGET").expect("cargo always sets TARGET for a build script");
    println!("cargo::rustc-env=ATMUX_TARGET={target}");
}
