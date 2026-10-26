# matter-peek-web

This crate is the web UI of [matter-peek](https://github.com/sysgrok/matter-peek),
written with Yew and compiled to Wasm. It is a WebSocket/JSON client of the
matter-peek API. The web UI is built into the library of `matter-peek-server`,
which serves it, and through that library into the `matter-peek` executable and
`matter-peek-app`. No other crate depends on this one; it holds the source of
the web UI.

```sh
# Needs nightly Rust with the wasm32 target (see rust-toolchain.toml), and trunk
cargo install trunk
trunk build --release        # builds dist/, which the server embeds
trunk serve                  # rebuilds on each change, and forwards the API requests to a server
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
