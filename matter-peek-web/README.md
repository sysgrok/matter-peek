# matter-peek-web

The web UI of [matter-peek](https://github.com/sysgrok/matter-peek) (Yew/Wasm):
a WebSocket/JSON client of its API. `matter-peek-server` serves it, built into
the server's library, and so do the `matter-peek` executable and
`matter-peek-app`; nothing depends on this crate, which is the UI's source.

```sh
# Nightly with the wasm32 target (rust-toolchain.toml), and trunk
cargo install trunk
trunk build --release        # dist/, which the server takes in
trunk serve                  # rebuilt on each change, the API passed on to a server
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
