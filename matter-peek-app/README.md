# matter-peek-app

[matter-peek](https://github.com/sysgrok/matter-peek) as a desktop or Android
app (Tauri): the web UI in a window, on a controller of its own in the app or on
a matter-peek server, asked at first launch and changed from the top bar.

```sh
# Linux only: the system's WebKitGTK and D-Bus, to build it (Windows'
# WebView2 and macOS' WKWebView come with the OS)
sudo apt install libwebkit2gtk-4.1-dev libdbus-1-dev

cargo install matter-peek-app
```

The Android build is in the project's
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
