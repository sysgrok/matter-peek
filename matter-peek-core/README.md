# matter-peek-core

The API of [matter-peek](https://github.com/sysgrok/matter-peek), `MatterPeek`,
and the Matter controller behind it, on
[rs-matter](https://github.com/project-chip/rs-matter): it commissions devices
onto its fabrics, lists them, reads and follows their health, retrieves their
logs, updates them and decommissions them.

Its methods are the API for Rust, for the mobile apps (UniFFI, the `uniffi`
feature) and, as wire messages (`api::wire`), for the WebSocket server; its MCP
tools are here too.

Without default features it is `no_std` + `alloc`: the API types only, which is
what the web UI takes. `controller` adds the controller and `std` runs it on
this host; the mDNS backends (`mdns-avahi`, the default, `mdns-resolve`,
`mdns-astro`, `mdns-builtin`) and `ble` pick how it finds and reaches devices.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
