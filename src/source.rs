use {
    std::{
        collections::{HashMap, HashSet},
        convert::TryInto,
        fs,
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

/// Get the temperature of a hard drive via smartctl. This function fails only
/// if smartctl fails to run or if the output can't be parsed as JSON. If the
/// SMART data does not include the temperature or if the reported temperature
/// exceeds the bounds of a `u8`, then `Ok(None)` is returned.
fn parse_smart_source<T: AsRef<Path>>(block_dev: T) -> Result<Option<u8>> {
    let mut proc = Command::new("smartctl")
        .arg("-j")
        .arg("-A")
        .arg("-n")
        .arg("standby")
        .arg(block_dev.as_ref())
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
        .map_err(|e| Error::SmartParse { block_dev: block_dev.as_ref().to_owned(), source: e })?;

    let temperature = root
        .get("temperature")
        .and_then(|v| v.get("current"))
        .and_then(|v| v.as_u64())
        .and_then(|v| v.try_into().ok());

    Ok(temperature)
}

/// Get the temperature from a plain-text file (typically a sysfs path). This
/// function only fails if the file cannot be read or if the whitespace-trimmed
/// contents cannot be parsed as an integer. If the reported temperature exceeds
/// the bounds of a `u8`, then `Ok(None)` is returned.
fn parse_file_source<T: AsRef<Path>>(path: T) -> Result<Option<u8>> {
    let contents = fs::read_to_string(path.as_ref())
        .map_err(|e| Error::Io { path: path.as_ref().to_owned(), source: e })?;
    let trimmed = contents.trim();

    // The file should be in milli-degrees Celsius
    let temperature = trimmed
        .parse::<u32>()
        .map_err(|e| Error::SensorValueParse { value: trimmed.to_owned(), source: e })?
        .checked_div(1000)
        .and_then(|t| t.try_into().ok());

    Ok(temperature)
}

/// Get the temperatures for the given list of sensors from IPMI. This queries
/// all temperature sensors and then filters the results. This function only
/// fails if the IPMI sensor query fails. If a sensor's unit is not degrees
/// Celsius or if the value exceeds the bounds of a `u8`, then the reported
/// value of that sensor will be `None`.
fn parse_ipmi_sources(ipmi: Arc<Mutex<Ipmi>>, sensors: &HashSet<String>)
    -> Result<HashMap<String, Option<u8>>>
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
        };

        result.insert(sensor.into(), temperature);
    }

    Ok(result)
}

/// Get temperature readings for the given sources. The returned values are in
/// the same order as given.
pub fn get_source_readings(ipmi: Arc<Mutex<Ipmi>>, sources: &[Source])
    -> Result<Vec<Option<u8>>>
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
            }
        })
        .collect()
}
