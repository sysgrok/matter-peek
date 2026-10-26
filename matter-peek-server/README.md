# matter-peek-server

The server of [matter-peek](https://github.com/sysgrok/matter-peek): a `Hub` (a
controller and its assistant, for the connections to them) and the API over
WebSocket/JSON with the web UI, on one port. The `matter-peek` executable
(`matter-peek serve`) and `matter-peek-app` embed it.

The web UI (`matter-peek-web`) is built into the library, gzipped: from
`MATTER_PEEK_WEB_DIST` if set, else from `../matter-peek-web/dist` in the
repository, else from `web-dist`, the copy kept in git and in the published
crate.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
