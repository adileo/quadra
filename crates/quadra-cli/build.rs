// The target the binary is built for (`aarch64-apple-darwin`, …): `quadra upgrade` downloads the
// binary of the same target.
fn main() {
    println!(
        "cargo:rustc-env=QUADRA_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
