//! Generates the Swift / Kotlin bindings of the built library:
//!
//! ```text
//! cargo rustc -p matter-peek-core --features uniffi --crate-type cdylib
//! cargo run -p matter-peek-core --features uniffi-cli --bin uniffi-bindgen -- \
//!     generate --library target/debug/libmatter_peek_core.so --language kotlin --out-dir out
//! ```

fn main() {
    uniffi::uniffi_bindgen_main()
}
