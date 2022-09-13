use {
    std::{
        io,
        num::ParseIntError,
        path::PathBuf,
        process::ExitStatus,
        result,
    },
    thiserror::Error,
    tokio::task::JoinError,
    crate::ipmi,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to parse config: {path:?}: {source}")]
    ConfigParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("Failed to validate config: {path:?}: {reason}")]
    ConfigValidation {
        path: PathBuf,
        reason: String,
    },
    #[error("Failed to parse sensor value: {value:?}: {source}")]
    SensorValueParse {
        value: String,
        source: ParseIntError,
    },
    #[error("Failed to parse SMART output for block device {block_dev:?}: {source}")]
    SmartParse {
        block_dev: PathBuf,
        source: serde_json::Error,
    },
    #[error("No sensors had valid temperature readings")]
    NoValidReadings,
    #[error("Failed to run {command:?}: {status}")]
    Command {
        command: PathBuf,
        status: ExitStatus,
    },
    #[error("Failed all {attempts} attempt(s); last attempt error: {source}")]
    RetriesFailed {
        attempts: u64,
        source: Box<Self>,
    },
    #[error("Internal retry error: {message}")]
    RetriesInternal {
        message: String,
    },
    #[error("IPMI error: {0}")]
    Ipmi(#[from] ipmi::Error),
    #[error("{path:?}: {source}")]
    Io {
        path: PathBuf,
        source: io::Error,
    },
    #[error("Zone monitor loop panicked: {0}")]
    LoopPanicked(#[source] JoinError),
}

impl From<retry::Error<Self>> for Error {
    fn from(value: retry::Error<Self>) -> Self {
        use retry::Error::{Internal, Operation};

        match value {
            Operation { error, total_delay: _, tries } => {
                Self::RetriesFailed { attempts: tries, source: Box::new(error) }
            }
            Internal(message) => Self::RetriesInternal { message }
        }
    }
}

pub type Result<T, E = Error> = result::Result<T, E>;
