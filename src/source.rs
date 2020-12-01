use std::{
    collections::HashMap,
    convert::TryInto,
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use snafu::ResultExt;

use crate::config::Source;
use crate::error::*;
use crate::ipmi::{Ipmi, SensorReading};

/// Get the temperature of a hard drive via smartctl. This function fails only
/// if smartctl fails to run or if the output can't be parsed as JSON. If the
/// SMART data does not include the temperature or if the reported temperature
/// exceeds the bounds of a `u8`, then `Ok(None)` is returned.
fn parse_smart_source<T: AsRef<Path>>(block_dev: T) -> Result<Option<u8>> {
    let proc = Command::new("smartctl")
        .arg("-j")
        .arg("-A")
        .arg("-n")
        .arg("standby")
        .arg(block_dev.as_ref())
        .stdout(Stdio::piped())
        .spawn()
        .context(IoError { path: "(smartctl)".to_owned() })?;

    let root: serde_json::Value = serde_json::from_reader(proc.stdout.unwrap())
        .context(SmartParseError { block_dev: block_dev.as_ref().to_owned() })?;

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
        .context(IoError { path: path.as_ref().to_owned() })?;
    let trimmed = contents.trim();

    // The file should be in milli-degrees Celsius
    let temperature = trimmed
        .parse::<u32>()
        .context(SensorValueParseError { value: trimmed.to_owned() })?
        .checked_div(1000)
        .and_then(|t| t.try_into().ok());

    Ok(temperature)
}

/// Get the temperatures for the given list of sensors from IPMI. This performs
/// one ipmitool query to reduce the IPMI round trips, which may be slow. This
/// function only fails if the IPMI sensor query fails. If a sensor's unit is
/// not degrees Celsius or if the value exceeds the bounds of a `u8`, then the
/// reported value of that sensor will be `None`.
fn parse_ipmi_sources<'a, T: AsRef<str>>(ipmi: Arc<Mutex<Ipmi>>, sensors: &'a [T])
    -> Result<HashMap<&'a str, Option<u8>>>
{
    if sensors.is_empty() {
        return Ok(HashMap::default());
    }

    let mut ipmi_lock = ipmi.lock().unwrap();
    let ipmi_readings = ipmi_lock.get_sensor_readings(sensors)
        .context(IpmiError)?
        .into_iter()
        .collect::<Result<Vec<SensorReading>, _>>()
        .context(IpmiError)?;

    let result = ipmi_readings
        .into_iter()
        .map(|r| {
            if r.units == "degrees C" {
                r.value.parse::<u8>().ok()
            } else {
                None
            }
        })
        .zip(sensors.iter())
        .map(|(temp, name)| (name.as_ref(), temp))
        .collect();

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
                Source::Ipmi { sensor } => Some(sensor),
                _ => None,
            }
        })
        .collect::<Vec<_>>();

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
