# Building the Linux host

The Linux host encodes through VAAPI, which needs three things at build time: the libva
headers, the library to link against, and libclang for the bindings generator. At run time it
needs the driver for the GPU and permission to open its render node.

On a machine you administer, one command covers the build side:

```sh
sudo apt install -y libva-dev clang pkg-config
```

and one covers the run side, because the render nodes belong to a group rather than to
everybody:

```sh
sudo usermod -aG render "$USER"   # then log in again
```

## Without root

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

## Which render node

A machine with two GPUs has two nodes and only one of them is the one you want. The vendor
identifier says which:

```sh
for n in /dev/dri/renderD*; do
  printf '%s -> ' "$n"; cat "/sys/class/drm/$(basename "$n")/device/vendor"
done
```

`0x8086` is Intel, `0x10de` is NVIDIA, `0x1002` is AMD. VAAPI encoding on Intel goes through
`iHD_drv_video.so`, which the `intel-media-va-driver` package provides.
