//! In-crate `uniffi-bindgen` so foreign bindings can be generated without a
//! separate `cargo install uniffi-bindgen-cli`:
//!
//!   cargo run --bin uniffi-bindgen -- generate --library <so> --language kotlin --out-dir android/kotlin/

fn main() {
    uniffi::uniffi_bindgen_main()
}
