use std::{
    io,
    num::ParseIntError,
    path::PathBuf,
    process::ExitStatus,
    result,
};

use snafu::Snafu;
use tokio::task::JoinError;

use crate::ipmi;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("Failed to parse config {:?}: {}", path, source))]
    ConfigParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[snafu(display("Failed to validate config {:?}: {}", path, reason))]
    ConfigValidation {
        path: PathBuf,
        reason: String,
    },
    #[snafu(display("Failed to parse sensor value: '{}': {}", value, source))]
    SensorValueParse {
        value: String,
        source: ParseIntError,
    },
    #[snafu(display("Failed to parse SMART output for block device {:?}: {}", block_dev, source))]
    SmartParse {
        block_dev: PathBuf,
        source: serde_json::Error,
    },
    #[snafu(display("No sensors had valid temperature readings"))]
    NoValidReadings,
    #[snafu(display("Failed to run {:?}: {}", command, status))]
    Command {
        command: PathBuf,
        status: ExitStatus,
    },
    #[snafu(display("IPMI error: {}", source))]
    Ipmi {
        source: ipmi::Error,
    },
    #[snafu(display("{:?}: {}", path, source))]
    Io {
        path: PathBuf,
        source: io::Error,
    },
    #[snafu(display("Zone monitor loop panicked: {}", source))]
    LoopPanicked {
        source: JoinError,
    },
}

pub type Result<T, E = Error> = result::Result<T, E>;
