use std::{
    error,
    process::Command,
    result,
};

use log::debug;
use rexpect::{
    errors,
    session::{PtyReplSession, spawn_command},
};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to parse line '{}': {}", line, source))]
    OutputParseError {
        line: String,
        source: Box<dyn error::Error>,
    },
    #[snafu(display("Failed to interact with ipmitool: {}", source))]
    InteractionError {
        source: errors::Error,
    },
    #[snafu(display("Invalid argument: '{}'", arg))]
    InvalidArgument {
        arg: String,
    },
    #[snafu(display("Output is desynced: expected '{}', but got '{}'", expected, got))]
    DesyncedOutput {
        expected: String,
        got: String,
    },
    #[snafu(display("Sensor not found: '{}'", name))]
    SensorNotFound {
        name: String,
    },
}

type Result<T, E = Error> = result::Result<T, E>;

#[derive(Clone, Copy, Debug)]
pub enum FanMode {
    Standard,       // 0
    Full,           // 1
    Optimal,        // 2
    HeavyIo,        // 4
    Unknown(u8),    // Anything else
}

impl From<u8> for FanMode {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Standard,
            1 => Self::Full,
            2 => Self::Optimal,
            4 => Self::HeavyIo,
            n => Self::Unknown(n),
        }
    }
}

impl From<FanMode> for u8 {
    fn from(mode: FanMode) -> Self {
        match mode {
            FanMode::Standard => 0,
            FanMode::Full => 1,
            FanMode::Optimal => 2,
            FanMode::HeavyIo => 4,
            FanMode::Unknown(n) => n,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SensorReading {
    pub name: String,
    pub value: String,
    pub units: String,
}

pub struct Ipmi {
    session: PtyReplSession,
}

impl Ipmi {
    pub fn new() -> Result<Self> {
        let mut command = Command::new("ipmitool");
        command.arg("shell");

        let mut session = PtyReplSession {
            echo_on: false,
            prompt: "ipmitool> ".to_string(),
            pty_session: spawn_command(command, Some(2000))
                .context(InteractionError)?,
            quit_command: Some("exit".to_string()),
        };

        session.wait_for_prompt()
            .context(InteractionError)?;

        Ok(Self { session })
    }

    /// Execute ipmitool command and return output
    fn execute(&mut self, command: &str) -> Result<String> {
        debug!("Running IPMI command: '{}'", command);

        self.session.send_line(command)
            .context(InteractionError)?;
        self.session.wait_for_prompt()
            .context(InteractionError)
    }

    /// Get fan mode
    pub fn get_fan_mode(&mut self) -> Result<FanMode> {
        let output = self.execute("raw 0x30 0x45 0")?;

        let raw_mode = u8::from_str_radix(output.trim(), 16)
            .map_err(|x| x.into())
            .context(OutputParseError { line: output })?;

        Ok(FanMode::from(raw_mode))
    }

    /// Set fan mode
    pub fn set_fan_mode(&mut self, mode: FanMode) -> Result<()> {
        self.execute(&format!("raw 0x30 0x45 1 {}", u8::from(mode)))?;

        Ok(())
    }

    /// Get duty cycle
    pub fn get_duty_cycle(&mut self, zone: u8) -> Result<u8> {
        let output = self.execute(&format!("raw 0x30 0x70 0x66 0 {}", zone))?;

        let dcycle = u8::from_str_radix(output.trim(), 16)
            .map_err(|x| x.into())
            .context(OutputParseError { line: output })?;

        Ok(dcycle)
    }

    /// Set duty cycle
    pub fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()> {
        self.execute(&format!("raw 0x30 0x70 0x66 1 {} {}", zone, dcycle))?;

        Ok(())
    }

    /// Get sensor readings
    pub fn get_sensor_readings<T: AsRef<str>>(&mut self, sensors: &[T])
        -> Result<Vec<Result<SensorReading>>>
    {
        let mut command = "sdr get".to_string();

        for sensor in sensors {
            let sensor = sensor.as_ref();

            if sensor.find('\'').is_some() {
                return Err(Error::InvalidArgument { arg: sensor.to_string() });
            }

            command.push_str(" '");
            command.push_str(sensor);
            command.push_str("'");
        }

        debug!("Running IPMI command: '{}'", command);

        self.session.send_line(&command)
            .context(InteractionError)?;

        let mut results = vec![];

        for sensor in sensors {
            let sensor = sensor.as_ref();

            let r = self.session.exp_regex(r#"(^|\n)(Sensor ID\s+:\s+|Unable to find sensor id ')"#)
                .context(InteractionError)?;

            let found = !r.1.trim_start().starts_with("Unable");
            let sensor_name = if found {
                self.session.exp_string(" (")
            } else {
                self.session.exp_char('\'')
            }.context(InteractionError)?;

            if sensor_name != *sensor {
                return Err(Error::DesyncedOutput {
                    expected: sensor.to_string(),
                    got: sensor_name,
                });
            }

            if found {
                self.session.exp_regex(r#"\n\s+Sensor Reading\s+:\s+"#)
                    .context(InteractionError)?;
                let (_, value) = self.session.exp_regex(r#"[\d\.]+"#)
                    .context(InteractionError)?;

                self.session.exp_regex(r#"^\s+\(\+/-\s+[\d\.]+\)\s+"#)
                    .context(InteractionError)?;
                let units = self.session.read_line()
                    .context(InteractionError)?;

                self.session.exp_regex(r#"\r?\n\r?\n"#)
                    .context(InteractionError)?;

                results.push(Ok(SensorReading {
                    name: sensor_name.to_string(),
                    value,
                    units,
                }));
            } else {
                results.push(Err(Error::SensorNotFound {
                    name: sensor_name.to_string(),
                }));
            }
        }

        self.session.wait_for_prompt()
            .context(InteractionError)?;

        Ok(results)
    }
}