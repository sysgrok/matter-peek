# matter-peek-core

This crate contains the API of [matter-peek](https://github.com/sysgrok/matter-peek),
`MatterPeek`, and the Matter controller behind it, which is built on
[rs-matter](https://github.com/project-chip/rs-matter). The controller
commissions devices onto its fabrics, lists them, reads and follows their
health, retrieves their logs, updates them and decommissions them.

The methods of `MatterPeek` are the API for Rust code, and for the mobile apps
over UniFFI (with the `uniffi` feature). The same API is also available as wire
messages (`api::wire`), which the WebSocket server uses. The MCP tools are
defined in this crate too.

Without default features, the crate is `no_std` + `alloc` and contains only the
API types; this is what the web UI uses. The `controller` feature adds the
controller, and `std` runs it on the host. The mDNS backend features
(`mdns-avahi`, which is the default, `mdns-resolve`, `mdns-astro` and
`mdns-builtin`) and the `ble` feature select how the controller finds and
reaches devices.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
