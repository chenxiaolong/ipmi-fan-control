mod compat;
mod config;
mod error;
mod source;
mod ipmi;

use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{self, Read},
    os::unix::net::UnixStream,
    path::PathBuf,
    process,
    u8,
};

use env_logger::{self, Env};
use gcd::Gcd;
use log::{debug, error, info};
use snafu::ResultExt;
use structopt::StructOpt;

use compat::FoldFirst;
use config::{Config, Interval, Source, Step, Zone, load_config};
use error::*;
use ipmi::{FanMode, Ipmi};
use source::get_source_readings;

struct IpmiSession {
    /// Session name (for logging only)
    name: String,
    /// IPMI session
    ipmi: Ipmi,
    /// Original fan mode
    orig_fan_mode: FanMode,
    /// Set these zones to dcycle 100% before restoring original fan mode
    restore_zones: Vec<u8>,
}

impl IpmiSession {
    pub fn new<N, I, S, R>(name: N, args: I, restore_zones: R) -> Result<Self>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        R: IntoIterator<Item = u8>,
    {
        let mut ipmi = Ipmi::with_args(args).context(IpmiError)?;
        let orig_fan_mode = ipmi.get_fan_mode().context(IpmiError)?;

        info!("[{}] Original fan mode: {:?}", name.as_ref(), orig_fan_mode);
        info!("[{}] Setting fan mode to: {:?}", name.as_ref(), FanMode::Full);

        ipmi.set_fan_mode(FanMode::Full)
            .context(IpmiError)?;

        Ok(Self {
            name: name.as_ref().to_owned(),
            ipmi,
            orig_fan_mode,
            restore_zones: restore_zones.into_iter().collect(),
        })
    }
}

impl Drop for IpmiSession {
    fn drop(&mut self) {
        for z in &self.restore_zones {
            info!("[{}] Setting zone {} duty cycle to 100%", self.name, z);
            match self.ipmi.set_duty_cycle(*z, 100) {
                Ok(_) => {}
                Err(e) => error!("[{}] Failed to set duty cycle: {}", self.name, e),
            }
        }

        info!("[{}] Restoring fan mode to: {:?}", self.name, self.orig_fan_mode);
        match self.ipmi.set_fan_mode(self.orig_fan_mode) {
            Ok(_) => {}
            Err(e) => error!("[{}] Failed to restore fan mode: {}", self.name, e),
        }
    }
}

struct MainApp {
    config: Config,
    cancellation: (UnixStream, UnixStream),
    tick_length: Interval,
    sessions: HashMap<String, IpmiSession>,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        // The GCD of all the intervals gives us the tick interval for the loop.
        let tick_length = config.zones
            .iter()
            .map(|z| z.interval)
            .fold_first_compat(|a, b| Interval(a.0.gcd(b.0)))
            .expect("No zones defined");

        let mut sessions = HashMap::new();

        for (name, args) in &config.sessions.0 {
            let restore_zones: Vec<_> = config.zones
                .iter()
                .filter(|z| &z.session.0 == name)
                .flat_map(|z| &z.ipmi_zones)
                .copied()
                .collect();

            // Don't waste resources if nothing would use the session
            if restore_zones.is_empty() {
                continue;
            }

            sessions.insert(name.clone(), IpmiSession::new(name, args, restore_zones)?);
        }

        let cancellation = UnixStream::pair()
            .context(IoError { path: "(cancellation socket)" })?;
        cancellation.0.set_read_timeout(Some(tick_length.to_duration()))
            .context(IoError { path: "(cancellation socket)" })?;

        Ok(Self {
            config,
            cancellation,
            tick_length,
            sessions,
        })
    }

    fn run(&mut self) -> Result<()> {
        info!("Starting fan control loop");

        let mut ticks = vec![0u8; self.config.zones.len()];

        loop {
            for (i, zone_config) in self.config.zones.iter().enumerate() {
                if ticks[i] == 0 {
                    // Guaranteed to exist during config validation
                    let session = self.sessions.get_mut(&zone_config.session.0).unwrap();
                    Self::update_duty_cycle(session, zone_config)?;
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
    fn update_duty_cycle(session: &mut IpmiSession, zone_config: &Zone) -> Result<()> {
        let max_cpu_temp = Self::get_max_temp(&mut session.ipmi, &zone_config.sources)?;

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
            let dcycle_cur = session.ipmi.get_duty_cycle(*z)
                .context(IpmiError)?;

            info!("[{}] Zone {}: cpu_temp={}C, dcycle_cur={}%, dcycle_new={}%",
                  session.name, z, max_cpu_temp, dcycle_cur, dcycle_new);

            session.ipmi.set_duty_cycle(*z, dcycle_new)
                .context(IpmiError)?;
        }

        Ok(())
    }

    /// Get maximum temperature sensor value in degrees Celsius.
    fn get_max_temp(ipmi: &mut Ipmi, sources: &[Source]) -> Result<u8> {
        get_source_readings(ipmi, sources)?
            .into_iter()
            .filter_map(|r| r)
            .max()
            .ok_or(Error::NoValidReadings)
    }
}

#[derive(StructOpt, Debug)]
struct Opt {
    /// Path to config file
    #[structopt(short, long)]
    config: PathBuf,
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
