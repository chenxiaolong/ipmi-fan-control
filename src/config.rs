use {
    std::{
        collections::HashMap,
        fmt,
        fs,
        path::Path,
        time::Duration,
    },
    retry::delay::Fixed,
    serde::Deserialize,
    crate::error::{Error, Result},
};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Warn => f.write_str("warn"),
            Self::Info => f.write_str("info"),
            Self::Debug => f.write_str("debug"),
            Self::Trace => f.write_str("trace"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Interval(pub u16);

impl Interval {
    pub fn to_duration(self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

impl Default for Interval {
    fn default() -> Self {
        Self(1)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Retries(pub usize);

impl Default for Retries {
    fn default() -> Self {
        Self(2)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RetryDelayMs(pub u64);

impl RetryDelayMs {
    pub fn to_fixed(self) -> Fixed {
        Fixed::from_millis(self.0)
    }
}

impl Default for RetryDelayMs {
    fn default() -> Self {
        Self(500)
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
    #[serde(default)]
    pub retries: Retries,
    #[serde(default)]
    pub retry_delay_ms: RetryDelayMs,
    pub ipmi_zones: Vec<u8>,
    pub sources: Vec<Source>,
    #[serde(default)]
    pub aggregation: Aggregation,
    pub steps: Vec<Step>,
}

impl Zone {
    pub fn retry_iter(&self) -> impl Iterator<Item = Duration> {
        self.retry_delay_ms.to_fixed().take(self.retries.0)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Sessions(pub HashMap<String, Vec<String>>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub sessions: Sessions,
    pub zones: Vec<Zone>,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .map_err(|e| Error::Io { path: path.to_owned(), source: e })?;

    let mut config: Config = toml::from_str(&contents)
        .map_err(|e| Error::ConfigParse { path: path.to_owned(), source: e })?;

    // Validate config

    // Create default session
    config.sessions.0.entry(SessionName::default().0)
        .or_insert_with(Vec::new);

    if config.zones.is_empty() {
        return Err(Error::ConfigValidation {
            path: path.to_owned(),
            reason: "zones: must be non-empty".to_owned(),
        });
    }

    for (i, zone_config) in config.zones.iter().enumerate() {
        if zone_config.interval.0 == 0 {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].interval: must be greater than 0", i),
            });
        }

        if zone_config.ipmi_zones.is_empty() {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].ipmi_zones: must be non-empty", i),
            });
        } else if zone_config.sources.is_empty() {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].sensors: must be non-empty", i),
            });
        }

        if !config.sessions.0.contains_key(&zone_config.session.0) {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].session: {:?} does not exist", i, zone_config.session.0),
            });
        }

        if matches!(zone_config.aggregation, Aggregation::Average { top: Some(0) }) {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].aggregation[type=average].top: must be greater than 0", i),
            });
        }

        for window in zone_config.steps.windows(2) {
            if window[0].temp >= window[1].temp {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].temp: values are not strictly increasing", i),
                });
            } else if window[0].dcycle > window[1].dcycle {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].dcycle: values are not increasing", i),
                });
            }
        }

        for (j, &step) in zone_config.steps.iter().enumerate() {
            if step.dcycle > 100 {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[{}].dcycle: invalid percentage: {}", i, j, step.dcycle),
                });
            }
        }
    }

    Ok(config)
}
