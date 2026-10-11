---
title: How to install reldir
---

# How to install reldir

This guide shows how to install the `reldir` command and the Python, Node.js
and Rust libraries. The command comes ready-built with every
[GitHub release](https://github.com/jowharshamshiri/reldir/releases); the
libraries are built from a clone of the repository: the `reldir` package on
crates.io is a different, incompatible version, and no package is published to
PyPI or npm.

## The command, from a release

Each release carries `reldir-v<version>-<target>.tar.gz` for Linux (x86-64 and
ARM64, glibc 2.35 or later), macOS (Intel and Apple silicon) and Windows
(x86-64, also as a `.zip`), and beside each `reldir-v<version>-<target>.sha256`,
its checksum. Download the two for your platform, check the one against the
other, unpack the archive, and put `reldir` on your `PATH`:

```sh
sha256sum -c reldir-v*-x86_64-unknown-linux-gnu.sha256
tar -xzf reldir-v*-x86_64-unknown-linux-gnu.tar.gz
install -m 755 reldir ~/.local/bin/
reldir --version
```

From then on, `reldir self update` replaces it with the latest release's
build, after checking the download against its checksum; see
[the CLI reference]({{ site.baseurl }}/cli#the-rest).

## The command, from source

Every build needs a Rust toolchain of version 1.89 or later
([rustup](https://rustup.rs) installs one). Clone the repository first:

```sh
git clone https://github.com/jowharshamshiri/reldir
cd reldir
```

```sh
cargo install --locked --path crates/reldir
```

This builds `reldir` and puts it in Cargo's binary directory,
`~/.cargo/bin` by default. Make sure that directory is on your `PATH`, then
confirm:

```sh
reldir --version
```

To replace an installed `reldir` with a newer checkout, pull and run the same
`cargo install` again -- or install a release over it with `reldir self update`.

## The Python library

The library needs Python 3.10 or later. Install it with `pip` into the
environment your application uses:

```sh
pip install ./crates/reldir-py
```

`pip` builds the engine with the Rust toolchain and installs the `reldir`
package together with the `reldir` command, so this also installs the command
into that environment. Confirm:

```sh
python -c 'import reldir; print(reldir.__version__)'
```

See [Python]({{ site.baseurl }}/python) for the library itself.

## The Node.js library

The library needs Node.js 22 or later. Build the native addon in the clone:

```sh
cd crates/reldir-node
npm ci
npm run build
```

Then, in your application's directory, install it by its path:

```sh
npm install /path/to/reldir/crates/reldir-node
```

`npm` links the package into `node_modules/@reldir/reldir`, so a rebuild of
the addon is seen by the application without installing again. Confirm:

```sh
node -e 'console.log(require("@reldir/reldir").version)'
```

See [Node.js]({{ site.baseurl }}/node) for the library itself.

## The Rust library

An application in Rust depends on the engine crate, `reldir-core`, by its path
in the clone:

```toml
[dependencies]
reldir-core = { path = "/path/to/reldir/crates/reldir-core" }
serde_json = "1"
```

See [Rust]({{ site.baseurl }}/rust) for the library itself.
