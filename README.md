ipmi-fan-control
================

ipmi-fan-control is a program written in Rust to control the fans of a SuperMicro rack mount server based on the readings of temperature sensors.

_Note_: This has only been tested on a 6028U-TR4T+, which uses the X10DRU-i+ motherboard. Also, only Linux is supported currently.

Installation
------------

Fedora packages are available at this official Copr repository: https://copr.fedorainfracloud.org/coprs/chenxiaolong/ipmi-fan-control/.

For other Linux distros or Unix-like systems, follow the instructions in the next section to build from source. Windows is currently not supported.

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

Running
-------

The only runtime dependency is `ipmitool`. It can be installed via your Linux distribution's package manager.

A config file is required for `ipmi-fan-control` to run. Make a copy of [`config.sample.toml`](config.sample.toml) and update the values to match your server's configuration. The configuration options are documented in the sample file.

Once you have a config file, `ipmi-fan-control` can be run with:

```sh
# Debug
sudo ./target/debug/ipmi-fan-control --config config.toml
# Release
sudo ./target/release/ipmi-fan-control --config config.toml
```

TODO
----

* Use libfreeipmi directly instead of interacting with `ipmitool shell` with rexpect. If the ipmitool shell UI changes, ipmi-fan-control will most likely break.
* Add support for Windows.
