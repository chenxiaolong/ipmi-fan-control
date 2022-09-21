use {
    std::{
        collections::{HashMap, HashSet},
        convert::TryInto,
        fs,
        io::{BufRead, BufReader},
        path::Path,
        process::{Command, Stdio},
        sync::{Arc, Mutex},
    },
    crate::{
        config::Source,
        error::{Error, Result},
        freeipmi::{SensorUnits, SensorValue},
        ipmi::Ipmi,
    },
};

/// Get the temperature of a hard drive via smartctl. This function fails if
/// smartctl does not return temperature data (eg. if a drive is in standby) or
/// if the reported temperature does not fit in a [`u8`].
fn parse_smart_source<T: AsRef<Path>>(block_dev: T) -> Result<u8> {
    let block_dev = block_dev.as_ref();

    let mut proc = Command::new("smartctl")
        .arg("-j")
        .arg("-A")
        .arg("-n")
        .arg("standby")
        .arg(block_dev)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io { path: "(smartctl)".into(), source: e })?;

    let result = serde_json::from_reader(proc.stdout.take().unwrap());
    let status = proc.wait()
        .map_err(|e| Error::Io { path: "(smartctl)".into(), source: e })?;

    match status.code() {
        // smartctl will return status code 2 when a drive is in standby
        Some(0) | Some(2) => {},
        _ => return Err(Error::Command { command: "smartctl".into(), status }),
     }

    let root: serde_json::Value = result
        .map_err(|e| Error::SmartParse { block_dev: block_dev.to_owned(), source: e })?;

    let temperature = root
        .get("temperature")
        .and_then(|v| v.get("current"))
        .ok_or_else(|| Error::SmartNoReading(block_dev.to_owned()))?
        .as_u64()
        .and_then(|v| v.try_into().ok())
        .ok_or(Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

/// Get the temperature of a Hitachi/HGST/WD drive via hdparm. This function
/// fails if hdparm does not print the temperature line, hdparm prints the bad
/// sense data line, or if the reported temperature does not fit in a [`u8`].
fn parse_hdparm_source<T: AsRef<Path>>(block_dev: T) -> Result<u8> {
    let block_dev = block_dev.as_ref();

    let mut proc = Command::new("hdparm")
        .arg("-H")
        .arg(block_dev)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io { path: "(hdparm)".into(), source: e })?;

    let mut reader = BufReader::new(proc.stdout.take().unwrap());
    let mut line = String::new();

    loop {
        let n = reader.read_line(&mut line)
            .map_err(|e| Error::Io { path: "(hdparm)".into(), source: e })?;
        if n == 0 {
            return Err(Error::HdparmNoData(block_dev.to_owned()));
        } else if line.contains("bad/missing sense data") {
            // hdparm exits with 0 when the drive responds with bad data
            return Err(Error::HdparmBadData(block_dev.to_owned()));
        } else if !line.contains("drive temperature (celsius) is:") {
            continue;
        }

        // The drive temperature line always has the number as the last token
        let last_token = line.trim_end().rsplit_once(' ')
            .ok_or_else(|| Error::HdparmBadData(block_dev.to_owned()))?
            .1;

        let temperature = last_token
            // Can be negative, but is within the bounds of 1 byte
            .parse::<i8>()
            .map_err(|e| Error::SensorValueParse { value: last_token.to_owned(), source: e })?
            .try_into()
            .map_err(|_| Error::ReadingExceedsBounds)?;

        return Ok(temperature);
    }
}

/// Get the temperature from a plain-text file (typically a sysfs path). The
/// contents of the file should be a decimal-formatted integer in units of
/// thousandths degrees Celsius after whitespace is trimmed. If the temperature,
/// after being converted to degrees Celsius, does not fit in a [`u8`], then
/// [`Error::ReadingExceedsBounds`] is returned.
fn parse_file_source<T: AsRef<Path>>(path: T) -> Result<u8> {
    let contents = fs::read_to_string(path.as_ref())
        .map_err(|e| Error::Io { path: path.as_ref().to_owned(), source: e })?;
    let trimmed = contents.trim();

    // The file should be in milli-degrees Celsius
    let temperature = trimmed
        .parse::<u32>()
        .map_err(|e| Error::SensorValueParse { value: trimmed.to_owned(), source: e })?
        .checked_div(1000)
        .and_then(|t| t.try_into().ok())
        .ok_or(Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

/// Get the temperatures for the given list of sensors from IPMI. This queries
/// all temperature sensors and then filters the results. This function only
/// fails if the IPMI sensor query fails. If a sensor's unit is not degrees
/// Celsius or if the value exceeds the bounds of a `u8`, then the reported
/// value of that sensor will be `None`.
fn parse_ipmi_sources(ipmi: Arc<Mutex<Ipmi>>, sensors: &HashSet<String>)
    -> Result<HashMap<String, u8>>
{
    if sensors.is_empty() {
        return Ok(HashMap::default());
    }

    let mut ipmi_lock = ipmi.lock().unwrap();
    let ipmi_readings = ipmi_lock.get_temperature_readings()?;
    let mut result = HashMap::new();

    for sensor in sensors {
        let reading = match ipmi_readings.get(sensor) {
            Some(r) => r,
            None => return Err(Error::SensorNotFound(sensor.into())),
        };

        let reading = match reading {
            Some(r) => r,
            None => return Err(Error::SensorNoReading(sensor.into())),
        };

        if reading.units != SensorUnits::Celsius {
            return Err(Error::SensorBadUnits {
                sensor: sensor.into(),
                units: reading.units,
            });
        }

        let temperature = match reading.value {
            SensorValue::Uint32(t) => t.try_into().ok(),
            SensorValue::Double(t) => (t as u32).try_into().ok(),
            v => return Err(Error::SensorBadValue {
                sensor: sensor.into(),
                value: v,
            }),
        }.ok_or(Error::ReadingExceedsBounds)?;

        result.insert(sensor.into(), temperature);
    }

    Ok(result)
}

/// Get temperature readings for the given sources. The returned values are in
/// the same order as given.
pub fn get_source_readings(ipmi: Arc<Mutex<Ipmi>>, sources: &[Source])
    -> Result<Vec<u8>>
{
    // Get IPMI sensor readings in one go for better performance.
    let ipmi_sensors = sources.iter()
        .filter_map(|s| {
            match s {
                Source::Ipmi { sensor } => Some(sensor.clone()),
                _ => None,
            }
        })
        .collect::<HashSet<_>>();

    let ipmi_results = parse_ipmi_sources(ipmi, &ipmi_sensors)?;

    sources.iter()
        .map(|s| {
            match s {
                Source::Ipmi { sensor } => Ok(ipmi_results[sensor.as_str()]),
                Source::File { path } => parse_file_source(path),
                Source::Smart { block_dev } => parse_smart_source(block_dev),
                Source::Hdparm { block_dev } => parse_hdparm_source(block_dev),
            }
        })
        .collect()
}
