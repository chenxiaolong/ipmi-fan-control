use {
    std::{
        ffi::OsStr,
        num::ParseIntError,
        process::Command,
        result,
    },
    log::trace,
    rexpect::{
        errors,
        session::{PtyReplSession, spawn_command},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to parse line {line:?}: {source}")]
    OutputParse {
        line: String,
        source: ParseIntError,
    },
    #[error("Failed to parse sensor output: {details}: {source}")]
    SensorParse {
        details: String,
        source: errors::Error,
    },
    #[error("Failed to spawn ipmitool: {0}")]
    Spawn(#[source] errors::Error),
    #[error("Failed to send ipmitool command: {0}")]
    SendCommand(#[source] errors::Error),
    #[error("ipmitool shell prompt not found: {0}")]
    PromptNotFound(#[source] errors::Error),
    #[error("Command exceeds maximum size of {size}: {command:?}")]
    CommandTooLong {
        size: usize,
        command: String,
    },
    #[error("Invalid argument: {0:?}")]
    InvalidArgument(String),
    #[error("Output is desynced: expected {expected:?}, but got {got:?}")]
    DesyncedOutput {
        expected: String,
        got: String,
    },
    #[error("Sensor reading not available")]
    ReadingNotAvailable,
    #[error("Sensor not found: {0:?}")]
    SensorNotFound(String),
}

type Result<T, E = Error> = result::Result<T, E>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    /// Createt an [`Ipmi`] instance with a set of ipmitool arguments. The
    /// arguments can be used to specify options, like `-I lanplus`, and should
    /// not contain the executable name (argv[0]) nor any subcommand.
    ///
    /// The `ipmitool` executable will be found in the `PATH` environment
    /// variable. `TERM=` will be set in the child process to prevent readline
    /// from outputting bracketed paste mode control sequences.
    pub fn with_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("ipmitool");
        command.args(args);
        command.arg("shell");
        command.env("TERM", "");

        Self::with_command(command)
    }

    /// Create an [`Ipmi`] instance from a specific [`Command`]. The command
    /// should include the arguments necessary to spawn an `ipmitool shell`
    /// instance.
    fn with_command(command: Command) -> Result<Self> {
        let mut session = PtyReplSession {
            echo_on: false,
            prompt: "ipmitool> ".to_string(),
            pty_session: spawn_command(command, Some(2000))
                .map_err(Error::Spawn)?,
            quit_command: Some("exit".to_string()),
        };

        session.wait_for_prompt()
            .map_err(Error::PromptNotFound)?;

        Ok(Self { session })
    }

    /// Quote an array of ipmitool shell arguments to form a valid command
    /// string.
    ///
    /// The shell's command parsing is pretty simple and has the following
    /// properties:
    /// * Each command must fit in a line
    /// * Each line is parsed as a byte string
    /// * Quotes are used to surround individual arguments and cannot be escaped
    /// * An unterminated quote causes an out-of-bounds read
    /// * Empty quoted arguments are ignored
    /// * All whitespace within quotes (as determined by isspace()) become spaces
    /// * The maxinum number of arguments is `EXEC_ARG_SIZE` (64) and any extra
    ///   arguments are silently ignored
    fn shell_quote<I, S>(args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // This includes the null terminator
        const EXEC_ARG_SIZE: usize = 64;

        let mut command = String::new();
        let mut num_args = 0;

        for arg in args {
            let arg = arg.as_ref();

            if arg.is_empty() {
                continue;
            }

            if arg.find(|c: char| c == '\'' || c == '"' || c == '\n').is_some() {
                return Err(Error::InvalidArgument(arg.to_string()));
            }

            if num_args > 0 {
                command.push(' ');
            }

            // Avoid quoting if possible to reduce chance of exceeding the
            // shell's buffer size
            if arg.find(|c: char| c.is_whitespace()).is_some() {
                command.push('\'');
                command.push_str(arg);
                command.push('\'');
            } else {
                command.push_str(arg);
            }

            num_args += 1;
        }

        if command.len() >= EXEC_ARG_SIZE {
            return Err(Error::CommandTooLong {
                size: EXEC_ARG_SIZE,
                command,
            });
        }

        Ok(command)
    }

    /// Execute an ipmitool command and return the output. The output includes
    /// all text up to, but not including the shell prompt.
    fn execute<I, S>(&mut self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let command = Self::shell_quote(args)?;
        trace!("Running IPMI command: {:?}", command);

        // Ensure we always wait for the prompt so that a failure does not
        // result in an output desync
        let send_ret = self.session.send_line(&command)
            .map_err(Error::SendCommand);
        let prompt_ret = self.session.wait_for_prompt()
            .map_err(Error::PromptNotFound);

        // Prefer the send error if that failed
        send_ret?;

        prompt_ret
    }

    /// Get the current fan mode.
    pub fn get_fan_mode(&mut self) -> Result<FanMode> {
        let output = self.execute(&["raw", "0x30", "0x45", "0"])?;

        let raw_mode = u8::from_str_radix(output.trim(), 16)
            .map_err(|e| Error::OutputParse { line: output, source: e })?;

        Ok(FanMode::from(raw_mode))
    }

    /// Set the fan mode.
    pub fn set_fan_mode(&mut self, mode: FanMode) -> Result<()> {
        self.execute(&["raw", "0x30", "0x45", "1", &u8::from(mode).to_string()])?;

        Ok(())
    }

    /// Get the current duty cycle. The valud should be in the range [0, 100],
    /// but is not guaranteed as this function returns the raw value supplied by
    /// the BMC.
    pub fn get_duty_cycle(&mut self, zone: u8) -> Result<u8> {
        let output = self.execute(&["raw", "0x30", "0x70", "0x66", "0", &zone.to_string()])?;

        let dcycle = u8::from_str_radix(output.trim(), 16)
            .map_err(|e| Error::OutputParse { line: output, source: e })?;

        Ok(dcycle)
    }

    /// Set the duty cycle. The valud should be in the range [0, 100], but this
    /// is not validated. The raw `dcycle` value will be sent to the BMC as-is.
    pub fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()> {
        self.execute(&["raw", "0x30", "0x70", "0x66", "1", &zone.to_string(), &dcycle.to_string()])?;

        Ok(())
    }

    /// Get the readings for the specified sensors. The items in the result are
    /// in the same order as the input. If a sensor is not found, the result for
    /// that sensor will be [`Error::SensorNotFound`]. If the ipmitool `sdr get`
    /// output cannot be parsed (eg. if the sensor reading does not include
    /// units), then the function will fail hard and return no results.
    pub fn get_sensor_readings<T: AsRef<str>>(&mut self, sensors: &[T])
        -> Result<Vec<Result<SensorReading>>>
    {
        // Ensure we always wait for the prompt so that a failure does not
        // result in an output desync
        let sensor_ret = self.get_sensor_readings_internal(sensors);
        let prompt_ret = self.session.wait_for_prompt()
            .map_err(Error::PromptNotFound);

        // Prefer reporting the sensor error
        let sensor_ret = sensor_ret?;
        prompt_ret?;

        Ok(sensor_ret)
    }

    fn get_sensor_readings_internal<T: AsRef<str>>(&mut self, sensors: &[T])
        -> Result<Vec<Result<SensorReading>>>
    {
        if sensors.is_empty() {
            return Ok(vec![]);
        }

        let mut args = vec!["sdr", "get"];
        args.extend(sensors.iter().map(|s| s.as_ref()));

        let command = Self::shell_quote(args)?;
        trace!("Running IPMI command: {:?}", command);

        self.session.send_line(&command)
            .map_err(Error::SendCommand)?;

        let mut results = vec![];

        macro_rules! e {
            ($details:expr) => {
                |e| Error::SensorParse { details: $details.to_string(), source: e }
            }
        }

        for sensor in sensors {
            let sensor = sensor.as_ref();

            let r = self.session.exp_regex(r#"(^|\n)(Sensor ID\s+:\s+|Unable to find sensor id ')"#)
                .map_err(e!("ID line not found"))?;

            let found = !r.1.trim_start().starts_with("Unable");
            let sensor_name = if found {
                self.session.exp_string(" (")
            } else {
                self.session.exp_char('\'')
            }.map_err(e!("Name not found"))?;

            if sensor_name != *sensor {
                return Err(Error::DesyncedOutput {
                    expected: sensor.to_string(),
                    got: sensor_name,
                });
            }

            if found {
                self.session.exp_regex(r#"\n\s+Sensor Reading\s+:\s+"#)
                    .map_err(e!("Reading line not found"))?;
                let (_, value) = self.session.exp_regex(r#"([\d\.]+|Not Available)"#)
                    .map_err(e!("Reading value not found"))?;
                trace!("Sensor {:?} reading value: {:?}", sensor_name, value);

                if value == "Not Available" {
                    return Err(Error::ReadingNotAvailable);
                }

                let (_, tolerance) = self.session.exp_regex(r#"^\s+\(\+/-\s+[\d\.]+\)\s+"#)
                    .map_err(e!("Reading tolerance not found"))?;
                trace!("Sensor {:?} reading tolerance: {:?}", sensor_name, tolerance);

                let units = self.session.read_line()
                    .map_err(e!("Reading units not found"))?;
                trace!("Sensor {:?} reading units: {:?}", sensor_name, units);

                self.session.exp_regex(r#"\r?\n\r?\n"#)
                    .map_err(e!("End marker not found"))?;

                results.push(Ok(SensorReading {
                    name: sensor_name.to_string(),
                    value,
                    units,
                }));
            } else {
                results.push(Err(Error::SensorNotFound(sensor_name.to_string())));
            }
        }

        Ok(results)
    }
}
