# Building rack.exe for Windows

Cross-compile from Linux without root or a MinGW toolchain, using [zig](https://ziglang.org/download/) as the linker through `cargo-zigbuild`. Everything is installed under your home directory.

```sh
rustup target add x86_64-pc-windows-gnu
mkdir -p ~/.local/opt/zig-0.17.0 ~/.local/bin && cd "$(mktemp -d)"
curl -fLO https://ziglang.org/download/0.17.0/zig-x86_64-linux-0.17.0.tar.xz
echo "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026  zig-x86_64-linux-0.17.0.tar.xz" | sha256sum -c -
tar -xJf zig-x86_64-linux-0.17.0.tar.xz -C ~/.local/opt/zig-0.17.0 --strip-components=1
ln -sf ~/.local/opt/zig-0.17.0/zig ~/.local/bin/zig          # ~/.local/bin must be on PATH
cargo install cargo-zigbuild --locked
cd - && cargo zigbuild --release --target x86_64-pc-windows-gnu -p rack
```

The result is `target/x86_64-pc-windows-gnu/release/rack.exe` (a PE32+ console executable). The zig version is 0.17.0 and the sha256 comes from the official [download index](https://ziglang.org/download/index.json); check it against the index again when you change the version.
