mod config;
mod error;
mod source;
mod ipmi;

use std::{
    cmp::{self, Reverse},
    collections::HashMap,
    ffi::OsStr,
    io,
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    u8,
};

use env_logger::{self, Env};
use futures::{
    future::{Abortable, AbortHandle},
    stream::FuturesUnordered,
};
use log::{debug, error, info};
use snafu::ResultExt;
use structopt::StructOpt;
use tokio::{
    task,
    time::sleep,
};
use tokio_stream::StreamExt;

use config::{Aggregation, Config, load_config, Step, Zone};
use error::*;
use ipmi::{FanMode, Ipmi};
use source::get_source_readings;

#[cfg(unix)]
async fn interrupted() -> io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    Ok(())
}

#[cfg(windows)]
async fn interrupted() -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    tokio::select! {
        _ = ctrl_break() => {},
        _ = ctrl_c() => {},
    }

    Ok(())
}

struct IpmiSession {
    /// Session name (for logging only)
    name: String,
    /// IPMI session
    ipmi: Arc<Mutex<Ipmi>>,
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
            ipmi: Arc::new(Mutex::new(ipmi)),
            orig_fan_mode,
            restore_zones: restore_zones.into_iter().collect(),
        })
    }
}

impl Drop for IpmiSession {
    fn drop(&mut self) {
        let mut ipmi_lock = self.ipmi.lock().unwrap();

        for z in &self.restore_zones {
            info!("[{}] Setting zone {} duty cycle to 100%", self.name, z);
            match ipmi_lock.set_duty_cycle(*z, 100) {
                Ok(_) => {}
                Err(e) => error!("[{}] Failed to set duty cycle: {}", self.name, e),
            }
        }

        info!("[{}] Restoring fan mode to: {:?}", self.name, self.orig_fan_mode);
        match ipmi_lock.set_fan_mode(self.orig_fan_mode) {
            Ok(_) => {}
            Err(e) => error!("[{}] Failed to restore fan mode: {}", self.name, e),
        }
    }
}

struct MainApp {
    config: Config,
    sessions: HashMap<String, Arc<IpmiSession>>,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
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

            sessions.insert(name.clone(), Arc::new(
                IpmiSession::new(name, args, restore_zones)?));
        }

        Ok(Self {
            config,
            sessions,
        })
    }

    /// Run asynchronous loops for each zone. Returns when interrupted via
    /// signal handlers (eg. ^C) or if a fatal error occurs.
    async fn run(&mut self) -> Result<()> {
        let mut loops = FuturesUnordered::new();
        let mut aborts = Vec::new();

        for zone_config in &self.config.zones {
            let (handle, registration) = AbortHandle::new_pair();

            loops.push(tokio::spawn(Abortable::new(
                Self::zone_loop(
                    self.sessions.get_mut(&zone_config.session.0).unwrap().clone(),
                    // Cloned since there's no structured concurrency support yet
                    Arc::new(zone_config.clone()),
                ),
                registration,
            )));
            aborts.push(handle);
        }

        let mut first_result = None;

        loop {
            let ret: Result<()> = tokio::select! {
                // Explicitly interrupted by ^C or signal handler
                c = interrupted() => {
                    if c.is_ok() {
                        info!("Interrupted");
                    }
                    c.context(IoError { path: "(interrupt)" })
                }
                // Oh boy, this is an Option<Result<Result<Result<()>, Aborted>, JoinError>>
                r = loops.next() => {
                    match r {
                        // No tasks left
                        None => break,
                        // The task panicked
                        Some(Err(e)) => Err(e).context(LoopPanicked),
                        // The task was aborted
                        Some(Ok(Err(_))) => Ok(()),
                        // zone_loop's actual error return value
                        Some(Ok(Ok(r))) => r,
                    }
                }
            };

            if first_result.is_none() {
                first_result = Some(ret);
            }

            // If tokio::select returned, then a loop exited or the program was
            // explicitly interrupted. Interrupt all remaining tasks and the
            // loop will exit once the FuturesUnordered is empty. This mechanism
            // is necessary because Tokio's JoinHandles do not cancel tasks when
            // they are dropped. Without the explicit aborts and joins, the
            // IpmiSession destructors might not run since the tasks would keep
            // the Arcs alive.
            for handle in &aborts {
                handle.abort();
            }
        }

        first_result.unwrap_or(Ok(()))
    }

    /// Main loop for a zone. The loop runs forever while the future is being
    /// polled.
    ///
    /// All communication with the IPMI is behind a mutex to avoid needing
    /// multiple IPMI sessions.
    async fn zone_loop(
        session: Arc<IpmiSession>,
        zone_config: Arc<Zone>,
    ) -> Result<()> {
        info!("[{}] Starting loop for IPMI zones {:?}",
              session.name, zone_config.ipmi_zones);

        loop {
            let s = session.clone();
            let z = zone_config.clone();

            task::block_in_place(move || {
                Self::update_duty_cycle(s, z.as_ref())
            })?;

            sleep(zone_config.interval.to_duration()).await;
        }
    }

    /// Update fan PWM duty cycle based on the CPU temperature
    fn update_duty_cycle(session: Arc<IpmiSession>, zone_config: &Zone) -> Result<()> {
        let temp = Self::get_temp(session.ipmi.clone(), zone_config)?;

        let result = zone_config.steps.binary_search_by(|s| s.temp.cmp(&temp));
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
                    temp,
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
            (u32::from(temp - below_step.temp)
                * u32::from(above_step.dcycle - below_step.dcycle)
                / u32::from(above_step.temp - below_step.temp)
                + u32::from(below_step.dcycle)) as u8
        };

        let mut ipmi_lock = session.ipmi.lock().unwrap();

        for z in &zone_config.ipmi_zones {
            let dcycle_cur = ipmi_lock.get_duty_cycle(*z)
                .context(IpmiError)?;

            info!("[{}] Zone {}: zone_temp={}C, dcycle_cur={}%, dcycle_new={}%",
                  session.name, z, temp, dcycle_cur, dcycle_new);

            ipmi_lock.set_duty_cycle(*z, dcycle_new)
                .context(IpmiError)?;
        }

        Ok(())
    }

    /// Get temperature sensor value in degrees Celsius using the zone's
    /// data aggregation method.
    fn get_temp(ipmi: Arc<Mutex<Ipmi>>, zone_config: &Zone) -> Result<u8> {
        let mut readings = get_source_readings(ipmi, &zone_config.sources)?
            .into_iter()
            .filter_map(|r| r)
            .collect::<Vec<_>>();
        readings.sort_by_key(|r| Reverse(*r));

        match zone_config.aggregation {
            Aggregation::Maximum => {
                readings.first().copied().ok_or(Error::NoValidReadings)
            }
            Aggregation::Average { top } => {
                let n = cmp::min(top.unwrap_or(readings.len()), readings.len());
                if n == 0 {
                    return Err(Error::NoValidReadings);
                }

                let sum = readings
                    .into_iter()
                    .take(n)
                    .map(|v| v as u32)
                    .sum::<u32>();

                Ok((sum as f32 / n as f32) as u8)
            }
        }
    }
}

#[derive(StructOpt, Debug)]
struct Opt {
    /// Path to config file
    #[structopt(short, long)]
    config: PathBuf,
}

async fn main_wrapper() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let opt = Opt::from_args();

    let config = load_config(&opt.config)?;
    debug!("Loaded config: {:#?}", config);

    let mut app = MainApp::new(config)?;
    app.run().await
}

#[tokio::main]
async fn main() {
    match main_wrapper().await {
        Ok(_) => {}
        Err(e) => {
            error!("{}", e);
            process::exit(1);
        }
    }
}
