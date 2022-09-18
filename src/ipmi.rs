use {
    std::{
        collections::HashMap,
        env,
        result,
    },
    log::trace,
    crate::{
        bindings,
        config::SessionType,
        freeipmi::{self, LfiSession, LimSession, SensorReading},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    FreeIpmi(#[from] freeipmi::Error),
    #[error("Expected response to be {expected} bytes, but have {actual} bytes")]
    BadResponseSize {
        expected: usize,
        actual: usize,
    }
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

const NET_FN_GENERIC: u8 = bindings::IPMI_NET_FN_OEM_SUPERMICRO_GENERIC_RQ as u8;
const CMD_FAN_MODE: u8 = 0x45;
const CMD_GENERIC_EXT: u8 = bindings::IPMI_CMD_OEM_SUPERMICRO_GENERIC_EXTENSION as u8;
const DATA_DUTY_CYCLE: u8 = 0x66;
const DATA_ACTION_READ: u8 = 0x0;
const DATA_ACTION_WRITE: u8 = 0x1;

// libipmimonitoring doesn't expose its underlying session and there's no way to
// give it an existing session, so we're stuck creating two connections.
pub struct Ipmi {
    lfi: LfiSession,
    lim: LimSession,
}

impl Ipmi {
    /// Createt an [`Ipmi`] instance for the given session type.
    pub fn new(st: &SessionType) -> Result<Self> {
        let lfi = LfiSession::new(st)?;
        let mut lim = LimSession::new(st)?;

        let temp_dir = env::temp_dir();
        trace!("SDR cache directory: {:?}", temp_dir);

        lim.set_sdr_cache_directory(&temp_dir)?;
        // This call is required, even if we're not loading a file
        lim.set_sensor_config_file(None)?;

        Ok(Self { lfi, lim })
    }

    /// Execute raw IPMI command and return the output. The output does not
    /// include the command number nor the status. If the command does not
    /// return a successful response or if the size of the response does not
    /// match the specified value, an error is returned.
    fn execute(
        &mut self,
        net_fn: u8,
        command: u8,
        data: &[u8],
        expected_size: usize,
    ) -> Result<Vec<u8>> {
        trace!("Running IPMI command: net_fn={:02x}, command={:02x}, data={:02x?}",
               net_fn, command, data);

        let response = self.lfi.raw_command(net_fn, command, data)?;

        if response.len() != expected_size {
            return Err(Error::BadResponseSize {
                expected: expected_size,
                actual: response.len(),
            });
        }

        Ok(response)
    }

    /// Get the current fan mode.
    pub fn get_fan_mode(&mut self) -> Result<FanMode> {
        let response = self.execute(
            NET_FN_GENERIC,
            CMD_FAN_MODE,
            &[DATA_ACTION_READ],
            1,
        )?;

        Ok(FanMode::from(response[0]))
    }

    /// Set the fan mode.
    pub fn set_fan_mode(&mut self, mode: FanMode) -> Result<()> {
        self.execute(
            NET_FN_GENERIC,
            CMD_FAN_MODE,
            &[
                DATA_ACTION_WRITE,
                mode.into(),
            ],
            0,
        )?;

        Ok(())
    }

    /// Get the current duty cycle. The valud should be in the range [0, 100],
    /// but is not guaranteed as this function returns the raw value supplied by
    /// the BMC.
    pub fn get_duty_cycle(&mut self, zone: u8) -> Result<u8> {
        let response = self.execute(
            NET_FN_GENERIC,
            CMD_GENERIC_EXT,
            &[
                DATA_DUTY_CYCLE,
                DATA_ACTION_READ,
                zone,
            ],
            1,
        )?;

        Ok(response[0])
    }

    /// Set the duty cycle. The valud should be in the range [0, 100], but this
    /// is not validated. The raw `dcycle` value will be sent to the BMC as-is.
    pub fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()> {
        self.execute(
            NET_FN_GENERIC,
            CMD_GENERIC_EXT,
            &[
                DATA_DUTY_CYCLE,
                DATA_ACTION_WRITE,
                zone,
                dcycle,
            ],
            0,
        )?;

        Ok(())
    }

    /// Get readings for all temperature sensors. If an error occurs, no partial
    /// results will be returned. If a temperature sensor has no reading, then
    /// the value in the result will be [`None`].
    pub fn get_temperature_readings(&mut self)
        -> Result<HashMap<String, Option<SensorReading>>> {
        let num_sensors = self.lim.temperature_sensor_readings()?;
        trace!("Number of sensors: {}", num_sensors);

        let mut result = HashMap::new();

        for _ in 0..num_sensors {
            result.insert(
                self.lim.read_sensor_name()?,
                self.lim.read_sensor()?,
            );

            self.lim.iterator_next()?;
        }

        Ok(result)
    }
}
