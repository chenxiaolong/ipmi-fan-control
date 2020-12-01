use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::Duration,
};

use serde::Deserialize;
use snafu::ResultExt;

use crate::error::*;

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Interval(pub u8);

impl Interval {
    pub fn to_duration(&self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

impl Default for Interval {
    fn default() -> Self {
        Self(1)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub temp: u8,
    pub dcycle: u8,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionName(pub String);

impl Default for SessionName {
    fn default() -> Self {
        Self("default".to_owned())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum Source {
    Ipmi {
        sensor: String,
    },
    File {
        // TOML can't encode OsString
        path: String,
    },
    Smart {
        // TOML can't encode OsString
        block_dev: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum Aggregation {
    Maximum,
    Average {
        top: Option<usize>,
    },
}

impl Default for Aggregation {
    fn default() -> Self {
        Self::Maximum
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    #[serde(default)]
    pub session: SessionName,
    #[serde(default)]
    pub interval: Interval,
    pub ipmi_zones: Vec<u8>,
    pub sources: Vec<Source>,
    #[serde(default)]
    pub aggregation: Aggregation,
    pub steps: Vec<Step>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Sessions(pub HashMap<String, Vec<String>>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sessions: Sessions,
    pub zones: Vec<Zone>,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .context(IoError { path })?;

    let mut config: Config = toml::from_str(&contents)
        .context(ConfigParseError { path })?;

    // Validate config

    // Create default session
    config.sessions.0.entry(SessionName::default().0)
        .or_insert(vec![]);

    if config.zones.is_empty() {
        return Err(Error::ConfigValidationError {
            path: path.to_owned(),
            reason: "zones: must be non-empty".to_owned(),
        });
    }

    for (i, ref zone_config) in config.zones.iter().enumerate() {
        if zone_config.interval.0 == 0 {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].interval: must be greater than 0", i),
            });
        }

        if zone_config.ipmi_zones.is_empty() {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].ipmi_zones: must be non-empty", i),
            });
        } else if zone_config.sources.is_empty() {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].sensors: must be non-empty", i),
            });
        }

        if !config.sessions.0.contains_key(&zone_config.session.0) {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].session: {:?} does not exist", i, zone_config.session.0),
            });
        }

        if matches!(zone_config.aggregation, Aggregation::Average { top: Some(0) }) {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].aggregation[type=average].top: must be greater than 0", i),
            });
        }

        for window in zone_config.steps.windows(2) {
            if window[0].temp >= window[1].temp {
                return Err(Error::ConfigValidationError {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].temp: values are not strictly increasing", i),
                });
            } else if window[0].dcycle > window[1].dcycle {
                return Err(Error::ConfigValidationError {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].dcycle: values are not increasing", i),
                });
            }
        }

        for (j, &step) in zone_config.steps.iter().enumerate() {
            if step.dcycle > 100 {
                return Err(Error::ConfigValidationError {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[{}].dcycle: invalid percentage: {}", i, j, step.dcycle),
                });
            }
        }
    }

    Ok(config)
}
