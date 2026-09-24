# Prism on Linux

A Linux machine can watch another machine and be watched, on x86-64 and on ARM. Everything that
makes that possible sits behind one Cargo feature, `linux-desktop`, and this page is what it
needs, what it does differently from the other two platforms, and what it does not do yet.

## What is different here

macOS and Windows keep every frame on the GPU from capture to screen. Linux does not, and that is
a choice rather than an oversight: the pieces that would let it are not on every Linux machine.

| | macOS / Windows | Linux |
|---|---|---|
| Screen | ScreenCaptureKit / Windows.Graphics.Capture | The desktop portal hands over a PipeWire stream |
| Encoder | VideoToolbox / NVENC or Media Foundation | OpenH264, on the processor |
| Decoder | VideoToolbox / Media Foundation | OpenH264, on the processor |
| Window | Metal / Direct3D 11 | SDL's renderer, with the picture uploaded as YUV |
| Input | CoreGraphics / `SendInput` | The desktop portal's remote desktop session |
| Codecs | H.264 and HEVC | H.264 only |

OpenH264 is built from source into the binary, so it is on every machine the build runs on — a
board, a virtual machine, a laptop whose GPU driver has no encoder. It is also why a Linux host
spends more processor than the others, and why a slow machine sharing a large screen will want a
lower frame rate or size in its settings.

### The first time a Linux machine is shared

A Wayland desktop gives its screen and its input to no application that simply asks. Prism asks
through `xdg-desktop-portal`, which shows the person at the machine one dialog covering both.
It asks when sharing is switched on, while somebody is there to answer, and it asks to be
remembered: the portal hands back a restore token, kept at `~/.prism/portal-token`, and every
session after that opens without a dialog. Revoking it in the desktop's settings, or deleting
that file, brings the question back.

A compositor that implements the screen cast portal but not the remote desktop one — some
wlroots desktops — shares its screen and not its input. The machine can be watched and not
controlled, and the host's log says so.

### What is not done yet

- **Sound from a Linux host.** A session from a Linux machine is silent. Playing sound as a
  client works.
- **The far pointer as its own layer.** The portal draws the pointer into the picture, because a
  Wayland client cannot read where the pointer is. So on a Linux host the pointer arrives with
  the picture, a frame late, rather than drawn by the client ahead of it.
- **VAAPI.** The encoder module under `encode/vaapi` holds the rate control and profile choices a
  hardware encoder would need and is not wired in. See [VAAPI](#vaapi) below.

## Building

On Ubuntu or Debian, one command installs everything the build compiles or links against:

```sh
sudo apt install build-essential clang cmake nasm pkg-config \
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libsoup-3.0-dev \
  libayatana-appindicator3-dev libpipewire-0.3-dev libspa-0.2-dev \
  libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxi-dev libxfixes-dev \
  libxss-dev libxtst-dev libwayland-dev libxkbcommon-dev libdecor-0-dev \
  libegl-dev libgl-dev libdrm-dev libgbm-dev libasound2-dev libpulse-dev libudev-dev
```

| For | Packages |
|---|---|
| The shell's window and tray | `libwebkit2gtk-4.1-dev` and the GTK, SVG and soup libraries beside it, `libayatana-appindicator3-dev` |
| The screen | `libpipewire-0.3-dev`, `libspa-0.2-dev`, and `clang` for the bindings |
| OpenH264 | `nasm`, on x86-64 only — ARM builds use OpenH264's own assembly |
| SDL, built from source into the stream window | `cmake`, and the headers of every display server and sound system SDL may load: X11 and its extensions, Wayland, xkbcommon, libdecor, EGL, GL, DRM, GBM, ALSA, PulseAudio, udev |

The SDL headers deserve the warning. SDL's build does not fail without, say, the Wayland headers;
it builds a stream window that cannot open on a Wayland desktop. `pnpm publish-local` checks for
them before it starts for exactly that reason.

Then, as on the other platforms:

```sh
pnpm install
pnpm publish-local
```

`publish-local` builds the shell with `linux-desktop` on — `crates/prism-tauri` asks for it on
Linux — and `scripts/sidecar.mjs` builds the stream window with `window,linux-desktop`.

### Checking Linux code from a Mac

`pnpm lint:cross` cannot build the Linux desktop: OpenH264, PipeWire's bindings and SDL all
compile C for the target, and a Mac has no Linux C toolchain. So it leaves out `prism-stream` and
`prism-cli` on Linux, as it already did on Windows, and CI's Linux jobs are what compile them.

Docker is the local answer. On Apple silicon a `linux/arm64` container runs natively, so an
Ubuntu image with the packages above builds and tests the Linux code at full speed:

```sh
cargo clippy -p prism-core -p prism-stream --features prism-stream/linux-desktop --all-targets
cargo test   -p prism-core -p prism-stream --features prism-stream/linux-desktop
```

Keep the container's `CARGO_TARGET_DIR` and `node_modules` away from the checkout the Mac uses:
a `pnpm install` run on a mounted checkout replaces the Mac's native tools with Linux ones.

### The tests that need a desktop

Two tests are marked ignored because they need a PipeWire daemon with a video source. A container
can provide one — the portal is the only part no test can stand in for, since it asks a person:

```sh
pipewire & wireplumber &
gst-launch-1.0 videotestsrc is-live=true \
  ! video/x-raw,format=BGRx,width=640,height=360,framerate=30/1 \
  ! pipewiresink mode=provide stream-properties="p,media.class=Video/Source" &

PRISM_TEST_PIPEWIRE_NODE=<the source's node id, from `pw-cli ls Node`> \
  cargo test -p prism-core --features linux-desktop -- --ignored
```

One reads frames through the capture; the other takes them through the whole host pipeline —
scaled into I420, encoded, decoded again.

## VAAPI

The VAAPI module is behind its own feature, `vaapi`, and needs three things at build time: the
libva headers, the library to link against, and libclang for the bindings generator. At run time
a VAAPI encoder would need the driver for the GPU and permission to open its render node.

```sh
sudo apt install -y libva-dev clang pkg-config
sudo usermod -aG render "$USER"   # then log in again
```

### Without root

Every build dependency can be staged into a home directory instead, which is worth knowing
because the machine with the interesting GPU is often not the machine you can install packages
on. `apt-get download` needs no privileges and `dpkg-deb -x` unpacks anywhere:

```sh
mkdir -p ~/opt/debs && cd ~/opt/debs
apt-get download libva2 libva-drm2 libva-dev intel-media-va-driver libigdgmm12 \
                 libclang1-19 libclang-common-19-dev libllvm19
for d in *.deb; do dpkg-deb -x "$d" ~/opt/root; done
```

Then point the build at them:

```sh
R=$HOME/opt/root
export CROS_LIBVA_H_PATH=$R/usr/include
export CROS_LIBVA_LIB_PATH=$R/usr/lib/x86_64-linux-gnu
export LIBCLANG_PATH=$R/usr/lib/x86_64-linux-gnu
export LD_LIBRARY_PATH=$R/usr/lib/x86_64-linux-gnu:$R/usr/lib/llvm-19/lib
export LIBVA_DRIVERS_PATH=$R/usr/lib/x86_64-linux-gnu/dri
export BINDGEN_EXTRA_CLANG_ARGS="-I$R/usr/lib/llvm-19/lib/clang/19/include"
export RUSTFLAGS="-L $R/usr/lib/x86_64-linux-gnu"
```

The one thing this cannot supply is a C compiler. Rust needs a linker driver, and `rust-lld`
alone does not know how to order the C runtime's startup objects — a binary linked that way
builds and then segfaults before reaching `main`, which is a worse failure than not building
at all. A machine with no `cc` needs `build-essential` installed properly.

The other thing it cannot supply is access to `/dev/dri/renderD*`. Those belong to the
`render` group, and group membership is not something a process can grant itself.

### Which render node

A machine with two GPUs has two nodes and only one of them is the one you want. The vendor
identifier says which:

```sh
for n in /dev/dri/renderD*; do
  printf '%s -> ' "$n"; cat "/sys/class/drm/$(basename "$n")/device/vendor"
done
```

`0x8086` is Intel, `0x10de` is NVIDIA, `0x1002` is AMD. VAAPI encoding on Intel goes through
`iHD_drv_video.so`, which the `intel-media-va-driver` package provides.
