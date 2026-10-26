# matter-peek-app

This crate is [matter-peek](https://github.com/sysgrok/matter-peek) as a
desktop or Android app, built with Tauri. It shows the web UI in a window. The
app either runs its own controller, or connects to a matter-peek server. This
is asked at the first launch, and can be changed later from the top bar.

```sh
# Linux only: the system's WebKitGTK and D-Bus are needed for the build
# (on Windows and macOS, WebView2 and WKWebView come with the OS)
sudo apt install pkg-config libwebkit2gtk-4.1-dev libdbus-1-dev

cargo install matter-peek-app
```

How to build the Android app is described in the project's
[build notes](https://github.com/sysgrok/matter-peek/blob/main/docs/BUILDING.md#the-app).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
