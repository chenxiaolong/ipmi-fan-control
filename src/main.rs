use std::{
    fs,
    io::{self, Read},
    num::ParseIntError,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process,
    result,
    time,
    u8,
};

use env_logger::{self, Env};
use log::{debug, error, info};
use serde::{Deserialize};
use signal_hook;
use snafu::{ResultExt, Snafu};
use structopt::StructOpt;
use toml;

mod ipmi;
use ipmi::{FanMode, Ipmi};

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Failed to load config {:?}: {}", path, source))]
    ConfigLoadError {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[snafu(display("No CPU temperature sensors found"))]
    NoCpuTempSensorsFound,
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

#[derive(Debug, Default)]
struct SensorReading {
    name: String,
    value: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct Zone {
    ipmi_zones: Vec<u8>,
    sensors: Vec<String>,
    min_temp: u8,
    max_temp: u8,
    min_dcycle: u8,
    max_dcycle: u8,
}

#[derive(Debug, Deserialize)]
struct Config {
    interval: Option<u8>,
    ipmitool_args: Option<Vec<String>>,
    zones: Vec<Zone>,
}

struct MainApp {
    config: Config,
    cancellation: (UnixStream, UnixStream),
    ipmi: Ipmi,
    orig_fan_mode: FanMode,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        let seconds = config.interval.unwrap_or(1);
        let interval = time::Duration::from_secs(seconds.into());

        let cancellation = UnixStream::pair()
            .context(IoError { path: "(cancellation socket)" })?;
        cancellation.0.set_read_timeout(Some(interval))
            .context(IoError { path: "(cancellation socket)" })?;

        let mut ipmi = Ipmi::new().context(IpmiError)?;
        let orig_fan_mode = ipmi.get_fan_mode().context(IpmiError)?;

        info!("Original fan mode: {:?}", orig_fan_mode);

        Ok(Self {
            config,
            cancellation,
            ipmi,
            orig_fan_mode,
        })
    }

    fn run(&mut self) -> Result<()> {
        info!("Setting fan mode to {:?}", FanMode::Full);
        self.ipmi.set_fan_mode(FanMode::Full)
            .context(IpmiError)?;

        info!("Starting fan control loop");

        loop {
            for zone_config in &self.config.zones {
                Self::update_duty_cycle(&mut self.ipmi, zone_config)?;
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

        let dcycle_val = if max_cpu_temp <= zone_config.min_temp {
            zone_config.min_dcycle
        } else if max_cpu_temp >= zone_config.max_temp {
            zone_config.max_dcycle
        } else {
            // Linear scaling
            ((max_cpu_temp - zone_config.min_temp) as u32
                * (zone_config.max_dcycle - zone_config.min_dcycle) as u32
                / (zone_config.max_temp - zone_config.min_temp) as u32
                + zone_config.min_dcycle as u32) as u8
        };

        for z in &zone_config.ipmi_zones {
            let dcycle = ipmi.get_duty_cycle(*z)
                .context(IpmiError)?;

            info!("- Zone {}: cpu_temp={}C, dcycle_cur={}%, dcycle_new={}%",
                  z, max_cpu_temp, dcycle, dcycle_val);

            ipmi.set_duty_cycle(*z, dcycle_val)
                .context(IpmiError)?;
        }

        Ok(())
    }

    /// Get maximum CPU temperature sensor value in degrees Celsius
    fn get_max_cpu_temp<T: AsRef<str>>(ipmi: &mut Ipmi, sensors: &[T]) -> Result<u8> {
        ipmi.get_sensor_readings(sensors)
            .context(IpmiError)?
            .into_iter()
            .map(|x| x.context(IpmiError) )
            .map(|x| x.and_then(|y| y.value.trim().parse::<u8>()
                .context(SensorValueParseError { value: y.value })))
            .collect::<Result<Vec<u8>>>()? // TODO: Can be avoided with itertools
            .into_iter()
            .max()
            .ok_or(Error::NoCpuTempSensorsFound)
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

    toml::from_str(&contents)
        .context(ConfigLoadError { path })
}

// TODO: Validate config
// min < max
// 0 <= dcycle <= 100
// interval > 0

fn main_wrapper() -> Result<()> {
    env_logger::from_env(Env::default().default_filter_or("info")).init();

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
