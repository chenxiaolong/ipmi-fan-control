use std::{
    io,
    num::ParseIntError,
    path::PathBuf,
    result,
};

use snafu::Snafu;

use crate::ipmi;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("Failed to parse config {:?}: {}", path, source))]
    ConfigParseError {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[snafu(display("Failed to validate config {:?}: {}", path, reason))]
    ConfigValidationError {
        path: PathBuf,
        reason: String,
    },
    #[snafu(display("Failed to parse sensor value: '{}': {}", value, source))]
    SensorValueParseError {
        value: String,
        source: ParseIntError,
    },
    #[snafu(display("Failed to parse SMART output for block device {:?}: {}", block_dev, source))]
    SmartParseError {
        block_dev: PathBuf,
        source: serde_json::Error,
    },
    #[snafu(display("No sensors had valid temperature readings"))]
    NoValidReadings,
    #[snafu(display("IPMI error: {}", source))]
    IpmiError {
        source: ipmi::Error,
    },
    #[snafu(display("{:?}: {}", path, source))]
    IoError {
        path: PathBuf,
        source: io::Error,
    },
}

pub type Result<T, E = Error> = result::Result<T, E>;
