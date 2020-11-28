mod compat;
mod ipmi;

use std::{
    fs,
    io::{self, Read},
    num::ParseIntError,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process,
    result,
    time::Duration,
    u8,
};

use env_logger::{self, Env};
use gcd::Gcd;
use log::{debug, error, info};
use serde::{Deserialize};
use snafu::{ResultExt, Snafu};
use structopt::StructOpt;

use compat::FoldFirst;
use ipmi::{FanMode, Ipmi};

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Snafu)]
enum Error {
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

type Result<T, E = Error> = result::Result<T, E>;

#[derive(Clone, Copy, Debug, Deserialize)]
struct Interval(u8);

impl Interval {
    fn to_duration(&self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

impl Default for Interval {
    fn default() -> Self {
        Self(1)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct Step {
    temp: u8,
    dcycle: u8,
}

#[derive(Debug, Deserialize)]
struct Zone {
    #[serde(default)]
    interval: Interval,
    ipmi_zones: Vec<u8>,
    sensors: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Config {
    ipmitool_args: Option<Vec<String>>,
    zones: Vec<Zone>,
}

struct MainApp {
    config: Config,
    cancellation: (UnixStream, UnixStream),
    tick_length: Interval,
    ipmi: Ipmi,
    orig_fan_mode: FanMode,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        // The GCD of all the intervals gives us the tick interval for the loop.
        let tick_length = config.zones
            .iter()
            .map(|z| z.interval)
            .fold_first_compat(|a, b| Interval(a.0.gcd(b.0)))
            .expect("No zones defined");

        let cancellation = UnixStream::pair()
            .context(IoError { path: "(cancellation socket)" })?;
        cancellation.0.set_read_timeout(Some(tick_length.to_duration()))
            .context(IoError { path: "(cancellation socket)" })?;

        let mut ipmi = Ipmi::new().context(IpmiError)?;
        let orig_fan_mode = ipmi.get_fan_mode().context(IpmiError)?;

        info!("Original fan mode: {:?}", orig_fan_mode);

        Ok(Self {
            config,
            cancellation,
            tick_length,
            ipmi,
            orig_fan_mode,
        })
    }

    fn run(&mut self) -> Result<()> {
        info!("Setting fan mode to {:?}", FanMode::Full);
        self.ipmi.set_fan_mode(FanMode::Full)
            .context(IpmiError)?;

        info!("Starting fan control loop");

        let mut ticks = vec![0u8; self.config.zones.len()];

        loop {
            for (i, zone_config) in self.config.zones.iter().enumerate() {
                if ticks[i] == 0 {
                    Self::update_duty_cycle(&mut self.ipmi, zone_config)?;
                }

                let max_ticks = zone_config.interval.0 / self.tick_length.0;
                ticks[i] = ticks[i].overflowing_add(1).0 % max_ticks;
            }

            let mut buf = [0];

            match self.cancellation.0.read_exact(&mut buf) {
                Ok(_) => {
                    info!("Stopping fan control loop");
                    break;
                },
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Interval expired
                },
                e @ Err(_) => {
                    return e.context(IoError { path: "(cancellation socket)" });
                }
            }
        }

        Ok(())
    }

    /// Write one byte to the pipe to cancel run()
    fn get_cancellation_pipe(&self) -> Result<UnixStream> {
        self.cancellation.1.try_clone()
            .context(IoError { path: "(cancellation socket)" })
    }

    /// Update fan PWM duty cycle based on the CPU temperature
    fn update_duty_cycle(ipmi: &mut Ipmi, zone_config: &Zone) -> Result<()> {
        let max_cpu_temp = Self::get_max_cpu_temp(ipmi, &zone_config.sensors)?;

        let result = zone_config.steps.binary_search_by(|s| s.temp.cmp(&max_cpu_temp));
        // Index of first step >= the current temperature (if exists)
        let above_index = match result {
            Ok(i) => Some(i),
            Err(i) if i == zone_config.steps.len() => None,
            Err(i) => Some(i),
        };
        // Index of first step < the current temperature (if exists)
        let below_index = match above_index {
            Some(0) => None,
            Some(i) => Some(i - 1),
            None => None,
        };
        // If step above doesn't exist, use last step's dcycle or 100%
        let above_step = match above_index {
            Some(i) => zone_config.steps[i],
            None => {
                let dcycle = zone_config.steps.last()
                    .map(|s| s.dcycle)
                    .unwrap_or(100);

                Step {
                    temp: max_cpu_temp,
                    dcycle,
                }
            }
        };
        // If step below doesn't exist, use same step as step above
        let below_step = match below_index {
            Some(i) => zone_config.steps[i],
            None => above_step,
        };

        let dcycle_new = if below_step.temp == above_step.temp {
            below_step.dcycle
        } else {
            // Linearly scale the dcycle
            (u32::from(max_cpu_temp - below_step.temp)
                * u32::from(above_step.dcycle - below_step.dcycle)
                / u32::from(above_step.temp - below_step.temp)
                + u32::from(below_step.dcycle)) as u8
        };

        for z in &zone_config.ipmi_zones {
            let dcycle_cur = ipmi.get_duty_cycle(*z)
                .context(IpmiError)?;

            info!("- Zone {}: cpu_temp={}C, dcycle_cur={}%, dcycle_new={}%",
                  z, max_cpu_temp, dcycle_cur, dcycle_new);

            ipmi.set_duty_cycle(*z, dcycle_new)
                .context(IpmiError)?;
        }

        Ok(())
    }

    /// Get maximum CPU temperature sensor value in degrees Celsius
    fn get_max_cpu_temp<T: AsRef<str>>(ipmi: &mut Ipmi, sensors: &[T]) -> Result<u8> {
        let temp = ipmi.get_sensor_readings(sensors)
            .context(IpmiError)?
            .into_iter()
            .map(|x| x.context(IpmiError) )
            .map(|x| x.and_then(|y| y.value.trim().parse::<u8>()
                .context(SensorValueParseError { value: y.value })))
            .collect::<Result<Vec<u8>>>()? // TODO: Can be avoided with itertools
            .into_iter()
            .max()
            .unwrap(); // Config validation guarantees there are sensors
        Ok(temp)
    }
}

impl Drop for MainApp {
    fn drop(&mut self) {
        for zone_config in &self.config.zones {
            for z in &zone_config.ipmi_zones {
                info!("Setting zone {} duty cycle to 100%", z);
                match self.ipmi.set_duty_cycle(*z, 100) {
                    Ok(_) => {}
                    Err(e) => error!("Failed to set duty cycle: {}", e),
                }
            }
        }

        info!("Restoring fan mode to: {:?}", self.orig_fan_mode);
        match self.ipmi.set_fan_mode(self.orig_fan_mode) {
            Ok(_) => {}
            Err(e) => error!("Failed to restore fan mode: {}", e),
        }
    }
}

#[derive(StructOpt, Debug)]
struct Opt {
    /// Path to config file
    #[structopt(short, long)]
    config: PathBuf,
}

fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .context(IoError { path })?;

    let config: Config = toml::from_str(&contents)
        .context(ConfigParseError { path })?;

    // Validate config

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
        } else if zone_config.sensors.is_empty() {
            return Err(Error::ConfigValidationError {
                path: path.to_owned(),
                reason: format!("zones[{}].sensors: must be non-empty", i),
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

fn main_wrapper() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let opt = Opt::from_args();

    let config = load_config(&opt.config)?;
    debug!("Loaded config: {:#?}", config);

    let mut app = MainApp::new(config)?;

    for signal in &[signal_hook::SIGINT, signal_hook::SIGTERM] {
        let socket = app.get_cancellation_pipe()?;

        signal_hook::pipe::register(*signal, socket)
            .context(IoError { path: "(cancellation socket)" })?;
    }

    app.run()
}

fn main() {
    match main_wrapper() {
        Ok(_) => {}
        Err(e) => {
            error!("{}", e);
            process::exit(1);
        }
    }
}
