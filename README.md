<p align="center"><img src="assets/framelight.svg" width="128" alt=""></p>

# framelight

A small app for the **Steam Frame** that streams what you see in the headset to a TV
or a computer. Press *Enable* on the headset, open `http://<headset>.local:8090` in any
browser on the same network, and watch. Nothing to install or pair on the watching side.

> [!NOTE]
> This is a vibe-coded hobby project. It works for me; I make no promises about the
> quality of the code. Use at your own risk.

## Install

In a terminal on the Steam Frame, or over `ssh`:

```sh
curl -fsSL https://github.com/VolanDeVovan/framelight/releases/latest/download/install.sh | sh
```

That puts the app in `~/.local/share/framelight/` and adds **framelight** to the app
list, next to the other non-Steam apps. No sudo, nothing outside your home directory,
nothing added to your Steam library. Run the same command again to update.

To remove it:

```sh
curl -fsSL https://github.com/VolanDeVovan/framelight/releases/latest/download/install.sh | sh -s -- --uninstall
```

## Use

1. Open **framelight** from the app list and press **Enable**. The window shows the
   address to open and how many people are watching.
2. On a TV, phone or computer on the same network, open that address
   (the IP address shown in the window, or `http://steamos.local:8090`).

Closing the window stops streaming. While it is disabled or nobody is watching, the
app does nothing at all.

**Good to know**

- Watch-only: no input, no remote play, no accounts.
- No password either: anyone on your network who opens the address can watch.
- The headset's Wi-Fi is the bottleneck, especially while it is also streaming PC VR.
  The page has a quality selector, and the bitrate drops by itself when packets get
  lost.
- Tested with Chromium-based browsers (including LG webOS TVs). Firefox and Safari
  have not been tried.

## Build from source

The headset is aarch64 with glibc 2.39, so the build is a cross build with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild). Any Linux machine
works. You need [rustup](https://rustup.rs), [zig](https://ziglang.org), cmake and a C
compiler; `rust-toolchain.toml` makes rustup fetch the aarch64 target by itself.

```sh
git clone https://github.com/VolanDeVovan/framelight
cd framelight
cargo install --locked cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39 -p framelight
packaging/dist.sh target/aarch64-unknown-linux-gnu/release/framelight dist
```

Nix users can run `nix develop` instead of installing the tools; the flake holds
only that development shell.

Then copy the archive and the installer to the headset and install from the file:

```sh
scp dist/framelight-aarch64-linux.tar.gz packaging/install.sh steamos@<headset>:
ssh steamos@<headset> sh install.sh framelight-aarch64-linux.tar.gz
```

Releases are built by GitHub Actions on every `v*` tag (`.github/workflows/release.yml`).

## How it works

The app grabs SteamVR's Headset View, encodes it with the headset's hardware H.264
encoder and sends it over WebRTC to every open browser tab, together with the
headset's sound. [DESIGN.md](DESIGN.md) has the details, measurements and the
reasoning behind them.

## License

MIT
