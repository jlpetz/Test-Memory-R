# TMR (Test Memory R)

**TMR** is a high-performance memory testing tool focused on DDR5 systems, built in Rust for speed, stability, and extensibility.

## Features

- Windows-native memory stress testing
- Multithreaded CPU core usage
- Extensible test patterns (MirrorMove, SimpleTest, etc.)
- A console (CLI) and, optionally, a window (GUI, WinUI 3 through `windows-reactor`) in one `tmr.exe`

## Windows support

`tmr.exe` opens its window when double-clicked, and runs in the console when started from a shell,
a script or a remote session; `--gui` and `--cli` choose explicitly. The window is built only with
`cargo build --release --features tmr-app/gui`; a plain `cargo build --release` is the console alone.

| Windows | Build | Console (CLI) | Window (GUI) |
|---|---|---|---|
| Server 2022 | 20348 | Yes | No: the console runs instead, and a double-click opens it in its own console window |
| Windows 11 21H2, 22H2, 23H2 | 22000–26099 | Yes | Yes, but a double-click flashes a console window briefly before the window opens |
| Windows 11 24H2 and later, Server 2025 | 26100 and later | Yes | Yes, fully supported |

The window also needs the Windows App Runtime 2.x installed; when it is missing, TMR shows a dialog
with the download link. Older Windows (Windows 10, Server 2019) is not supported.

## License

This project is licensed under either of:

- MIT License ([LICENSE-MIT](./LICENSE-MIT))
- Apache 2.0 License ([LICENSE-APACHE](./LICENSE-APACHE))

at your option.
