ipmi-fan-control
================

ipmi-fan-control is a program written in Rust to control the fans on Supermicro motherboards based on the readings of temperature sensors.

_Note_: This has primarily been tested on a 6028U-TR4T+, which uses the X10DRU-i+ motherboard. Also, only Linux and other Unix-like operating systems are currently supported.

**2022-11-04 Update**: This project is no longer in active development because I no longer have a Supermicro motherboard that requires more granular fan control. However, the project is feature complete and I will continue to do maintenance work, like updating dependencies and creating binary repos for new Linux distro releases. The underlying fan control IPMI commands have not changed across 5 generations of motherboards. It's unlikely that ipmi-fan-control will stop working on any currently supported motherboards.

**2024-04-14 Update**: I am archiving this repo and no longer plan to keep the binary repos up to date for new distro releases. Folks who want to continue using this project should build it from source. As mentioned in the previous update, this project is feature complete and is unlikely to suddenly stop working.

Installation
------------

Prebuilt packages for Arch Linux, CentOS, Fedora, openSUSE, Debian, and Ubuntu are available from [OBS](https://build.opensuse.org/package/show/home:chenxiaolong:ipmi-fan-control/ipmi-fan-control). Please follow the instructions at [the repo's landing page](https://software.opensuse.org//download.html?project=home%3Achenxiaolong%3Aipmi-fan-control&package=ipmi-fan-control).

For other Unix-like systems, follow the instructions in the next section to build from source. Windows is currently not supported.

Building
--------

This project depends on:

* the freeipmi suite of libraries (specifically, libfreeipmi and libipmimonitoring)
* `pkg-config`
* the Clang compiler (for generating Rust FFI bindings to the freeipmi libraries)
* the Rust compiler
* [optional] smartmontools (for querying HDD/SSD drive temperatures)
* [optional] hdparm (for querying Hitachi/HGST/WD drive temperatures while spun down)

These packages can be installed from the system package manager:

```sh
# Fedora
sudo dnf install freeipmi-devel pkgconf-pkg-config clang-devel cargo
# OpenSUSE
sudo zypper in freeipmi-devel pkgconf-pkg-config clang-devel cargo
# Arch Linux
sudo pacman -S freeipmi pkgconf clang cargo
# Debian-based distros
sudo apt install libfreeipmi-dev libipmimonitoring-dev pkg-config libclang-dev cargo
```

Then, to make a debug build, run:

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
