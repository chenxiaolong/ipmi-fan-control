ipmi-fan-control
================

ipmi-fan-control is a program written in Rust to control the fans on SuperMicro motherboards based on the readings of temperature sensors.

_Note_: This has primarily been tested on a 6028U-TR4T+, which uses the X10DRU-i+ motherboard. Also, only Linux and other Unix-like operating systems are currently supported.

Installation
------------

Prebuilt packages for Arch Linux, CentOS, Fedora, openSUSE, Debian, and Ubuntu are available from [OBS](https://build.opensuse.org/package/show/home:chenxiaolong:ipmi-fan-control/ipmi-fan-control). Please follow the instructions at [the repo's landing page](https://software.opensuse.org//download.html?project=home%3Achenxiaolong%3Aipmi-fan-control&package=ipmi-fan-control).

For other Unix-like systems, follow the instructions in the next section to build from source. Windows is currently not supported.

Building
--------

The project can be built with the normal cargo tool. To make a debug build, run:

```sh
cargo build
```

or to make a release build, run:

```sh
cargo build --release
```

To build Linux distro-specific packages, first build the corresponding source package:

```sh
# SRPM for RPM-based distros
./dist/build_source_package.py -t srpm
# PKGBUILD for Arch Linux
./dist/build_source_package.py -t pkgbuild
# dsc for Debian-based distros
./dist/build_source_package.py -t debian
```

and then use the distro's standard utilities for building the binary packages. The source packages will be placed in `dist/output/`.

Running
-------

If ipmi-fan-control was installed from a package, update `/etc/ipmi-fan-control.toml` to match the desired configuration and then enable and start the `ipmi-fan-control` systemd service.

If built from source, make a copy of [`config.sample.toml`](config.sample.toml) and update the values to match your server's configuration. Then, run `ipmi-fan-control` with:

```sh
# Debug
sudo ./target/debug/ipmi-fan-control --config config.toml
# Release
sudo ./target/release/ipmi-fan-control --config config.toml
```
