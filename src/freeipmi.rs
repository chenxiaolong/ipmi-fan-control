use {
    std::{
        cmp::Ordering,
        convert::TryInto,
        ffi::{CStr, CString},
        os::raw::{c_char, c_int, c_uint},
        path::Path,
        ptr,
        result,
        str::Utf8Error,
        sync::Mutex,
    },
    once_cell::sync::Lazy,
    crate::{
        bindings,
        config::SessionType,
    },
};

#[cfg(windows)]
use std::path::PathBuf;

#[derive(Debug, Eq, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("Failed to parse as UTF-8: {0}")]
    NotUtf8(#[from] Utf8Error),
    #[cfg(windows)]
    #[error("Path is not valid UTF-8: {0:?}")]
    PathNotUtf8(PathBuf),
    #[error("[libipmimonitoring] Failed to {action}: {message}")]
    Lim {
        action: &'static str,
        message: &'static str,
    },
    #[error("[libfreeipmi] Failed to {action}: {message}")]
    Lfi {
        action: &'static str,
        message: &'static str,
    },
    #[error("No in-band IPMI devices found")]
    InBandDeviceNotFound,
    #[error("Command response is too short")]
    ResponseTooShort,
    #[error("Requested command {request:02x}, but responded with command {response:02x}")]
    BadResponseCommand {
        request: u8,
        response: u8,
    },
    #[error("Failed to execute command: {0}")]
    CommandFailed(String),
}

type Result<T, E = Error> = result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SensorValue {
    Bool(bool),
    Uint32(u32),
    Double(f64),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SensorUnits {
    Celsius,
    Fahrenheit,
    Unknown(c_uint),
}

#[derive(Clone, Copy, Debug)]
pub struct SensorReading {
    pub value: SensorValue,
    pub units: SensorUnits,
}

/// Try to convert a pointer to a statically allocated C string to a UTF-8 Rust
/// string. Both LIM and LFI return error messages allocated from static
/// globals. This is documented behavior of the ipmi_*_strerror() and
/// ipmi_*_errormsg() functions.
unsafe fn from_static_utf8_cstr(ptr: *const i8) -> Result<&'static str> {
    Ok(CStr::from_ptr(ptr).to_str()?)
}

/// Low-level wrapper for libfreeipmi context.
struct LfiCtx(*mut bindings::ipmi_ctx);

impl LfiCtx {
    fn new() -> Result<Self> {
        // [Unsafe] No memory safety concerns
        let ctx = unsafe { bindings::ipmi_ctx_create() };
        if ctx.is_null() {
            return Err(Error::Lim {
                action: "create context",
                message: "(unknown)",
            });
        }

        Ok(Self(ctx))
    }

    fn error_msg(&self) -> Result<&'static str> {
        // [Unsafe] Always returns a valid string from a statically allocated
        // region
        unsafe {
            from_static_utf8_cstr(bindings::ipmi_ctx_errormsg(self.0))
        }
    }

    /// Find the local in-band IPMI device and use it for further calls with
    /// this context instance. Probing is enabled for automatically detecting
    /// the appropriate driver to use.
    #[allow(clippy::comparison_chain)]
    fn find_in_band(&mut self) -> Result<()> {
        // [Unsafe] No memory safety concerns
        let ret = unsafe {
            bindings::ipmi_ctx_find_inband(
                self.0,
                ptr::null_mut(),
                0,
                0,
                0,
                ptr::null_mut(),
                0,
                0,
            )
        };
        match ret.cmp(&0) {
            Ordering::Less => Err(Error::Lfi {
                action: "find inband IPMI device",
                message: self.error_msg()?,
            }),
            Ordering::Equal => Err(Error::InBandDeviceNotFound),
            Ordering::Greater => Ok(()),
        }
    }

    /// Connect to the specified out-of-band IPMI 2.0 device and use it for
    /// further calls with this context instance. The connection will use the
    /// admin privilege level and libfreeipmi's default connection timeouts.
    fn open_out_of_band(
        &mut self,
        hostname: &str,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let hostname_cstr = CString::new(hostname).unwrap();
        let username_cstr = CString::new(username).unwrap();
        let password_cstr = CString::new(password).unwrap();

        // [Unsafe] freeipmi stores its own copy of these strings in
        // buffers within ctx. It performs its own max length checks.
        let ret = unsafe {
            bindings::ipmi_ctx_open_outofband_2_0(
                self.0,
                hostname_cstr.as_ptr(),
                username_cstr.as_ptr(),
                password_cstr.as_ptr(),
                ptr::null(),
                0,
                // Use the same defaults as libipmimonitoring
                bindings::IPMI_PRIVILEGE_LEVEL_ADMIN.try_into().unwrap(),
                3,
                0,
                0,
                0,
                bindings::IPMI_FLAGS_DEFAULT,
            )
        };
        if ret < 0 {
            return Err(Error::Lfi {
                action: "open out-of-band IPMI device",
                message: self.error_msg()?,
            });
        }

        Ok(())
    }

    /// Execute a raw IPMI command. The first byte of the request buffer should
    /// be the command number, followed by any data if needed. The response
    /// buffer will contain the command number in the first byte and the status
    /// code in the second byte, followed by any response data. If the status
    /// code does not report a successful execution, [`Error::CommandFailed`] is
    /// returned. The size of both the request and response buffers must not
    /// exceed the bounds of a [`c_int`] or else the function will panic.
    fn raw_command(
        &mut self,
        lun: u8,
        net_fn: u8,
        request: &[u8],
        response: &mut [u8],
    ) -> Result<usize> {
        // [Unsafe] Shouldn't be any memory safety concerns. We're passing in
        // the size of all buffers.
        let ret = unsafe {
            bindings::ipmi_cmd_raw(
                self.0,
                lun,
                net_fn,
                request.as_ptr().cast(),
                request.len().try_into().expect("request buffer too large"),
                response.as_mut_ptr().cast(),
                response.len().try_into().expect("response buffer too large"),
            )
        };
        if ret < 0 {
            return Err(Error::Lfi {
                action: "execute raw command",
                message: self.error_msg()?,
            });
        }

        Ok(ret as usize)
    }
}

impl Drop for LfiCtx {
    fn drop(&mut self) {
        // [Unsafe] Context is always valid (non-NULL). This implicitly calls
        // ipmi_ctx_close() if a device is open.
        unsafe { bindings::ipmi_ctx_destroy(self.0) }
    }
}

/// The internal pointer is safe to move between threads.
unsafe impl Send for LfiCtx {}

/// High-level wrapper for libfreeipmi.
pub struct LfiSession(LfiCtx);

impl LfiSession {
    pub fn new(st: &SessionType) -> Result<Self> {
        // [Unsafe] No memory safety concerns
        let mut ctx = LfiCtx::new()?;

        match st {
            SessionType::Local => {
                ctx.find_in_band()?;
            }
            SessionType::Remote { hostname, username, password } => {
                ctx.open_out_of_band(hostname, username, &password.0)?;
            },
        };

        Ok(Self(ctx))
    }

    /// Execute a raw IPMI command. The return value only includes the response
    /// data, excluding the first two bytes (command number and status code). If
    /// the command fails, [`Error::CommandFailed`] is returned. The size of the
    /// request data must not be greater than or equal to 256 bytes or else the
    /// function will panic.
    pub fn raw_command(
        &mut self,
        net_fn: u8,
        command: u8,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        // Same as in freeipmi's ipmi-oem
        const IPMI_OEM_MAX_BYTES: usize = 256;
        const IPMI_OEM_ERR_BUFLEN: usize = 1024;

        assert!(data.len() < IPMI_OEM_MAX_BYTES);

        let mut request_buf = [0u8; IPMI_OEM_MAX_BYTES];
        let mut response_buf = [0u8; IPMI_OEM_MAX_BYTES];

        request_buf[0] = command;
        request_buf[1..=data.len()].copy_from_slice(data);

        let size = self.0.raw_command(
            0,
            net_fn,
            &request_buf[0..=data.len()],
            &mut response_buf,
        )?;

        if size < 2 {
            return Err(Error::ResponseTooShort);
        }

        if response_buf[0] != command {
            return Err(Error::BadResponseCommand {
                request: command,
                response: response_buf[0],
            });
        }

        if response_buf[1] != bindings::IPMI_COMP_CODE_COMMAND_SUCCESS as u8 {
            let mut error_buf = [0 as c_char; IPMI_OEM_ERR_BUFLEN + 1];

            // [Unsafe] Calls snprintf to fill error_buf. The buffer is zero-
            // initialized and one element larger, so it's guaranteed to have a
            // NULL terminator.
            let error_ret = unsafe {
                bindings::ipmi_completion_code_strerror_r(
                    command,
                    net_fn,
                    response_buf[1],
                    error_buf.as_mut_ptr(),
                    (error_buf.len() - 1).try_into().unwrap(),
                )
            };
            if error_ret < 0 {
                return Err(Error::CommandFailed(
                    format!("Unknown status: {:02x}", response_buf[1])));
            }

            // [Unsafe] Guaranteed to be NULL terminated
            let error_cstr = unsafe { CStr::from_ptr(error_buf.as_ptr()) };

            return Err(Error::CommandFailed(error_cstr.to_str()?.to_owned()));
        }

        Ok(response_buf[2..size].to_vec())
    }
}

/// Convert a libipmimonitoring error code to a string.
fn lim_strerror(errnum: c_int) -> Result<&'static str> {
    // [Unsafe] Always returns a valid string from a statically allocated region
    unsafe {
        from_static_utf8_cstr(bindings::ipmi_monitoring_ctx_strerror(errnum))
    }
}

static LIM_INITIALIZED: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

/// Initialize libipmimonitoring globally. This is thread safe and is a no-op if
/// LIM is already initialized.
fn lim_init() -> Result<()> {
    let mut initialized = LIM_INITIALIZED.lock().unwrap();

    if !*initialized {
        let mut errnum = 0i32;
        // [Unsafe] No memory safety concerns
        let ret = unsafe { bindings::ipmi_monitoring_init(0, &mut errnum) };
        if ret < 0 {
            return Err(Error::Lim {
                action: "initialize library",
                message: lim_strerror(errnum)?,
            });
        }

        *initialized = true;
    }

    Ok(())
}

/// Convert a path to a C string. If the path is not valid UTF-8, then an error
/// will be returned.
#[cfg(windows)]
fn path_to_cstring(path: &Path) -> Result<CString> {
    // Require that the path is valid UTF-8 because there's no reasonable way to
    // go from a WTF-8 OsStr to CString on Windows
    let path_str = path.to_str()
        .ok_or_else(|| Error::PathNotUtf8(path.to_owned()))?;

    // Safe to unwrap since a path never has embedded null bytes
    Ok(CString::new(path_str.as_bytes()).unwrap())
}

/// Convert a path to a C string. Never fails on Unix-like systems.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn path_to_cstring(path: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    // Safe to unwrap since a path never has embedded null bytes
    Ok(CString::new(path.as_os_str().as_bytes()).unwrap())
}

/// High-level wrapper for limipmimonitoring.
pub struct LimSession {
    ctx: *mut bindings::ipmi_monitoring_ctx,
    config: bindings::ipmi_monitoring_ipmi_config,
    hostname: Option<String>,
}

impl LimSession {
    pub fn new(st: &SessionType) -> Result<Self> {
        lim_init()?;

        // These two strings will be "owned" by the C struct and will be freed
        // in the Drop implementation. This allows LimSession to remain movable.
        let (hostname, username, password) = match st {
            SessionType::Local => (None, ptr::null_mut(), ptr::null_mut()),
            SessionType::Remote { hostname, username, password } => (
                Some(hostname.clone()),
                CString::new(username.as_str()).unwrap().into_raw(),
                CString::new(password.0.as_str()).unwrap().into_raw(),
            ),
        };

        // [Unsafe] No memory safety concerns. This will never leak because no
        // code below this point can panic.
        let ctx = unsafe { bindings::ipmi_monitoring_ctx_create() };
        if ctx.is_null() {
            return Err(Error::Lim {
                action: "create context",
                message: "(unknown)",
            });
        }

        let config = bindings::ipmi_monitoring_ipmi_config {
            // In-band options. All options are set to the default because we
            // use automatic probing. The documentation says that setting the
            // driver type to < 0 is equivalent to using the default of
            // IPMI_MONITORING_DRIVER_TYPE_KCS, but this is incorrect and the
            // implementation actually calls ipmi_ctx_find_inband.
            driver_type: -1,
            disable_auto_probe: 0,
            driver_address: 0,
            register_spacing: 0,
            driver_device: ptr::null_mut(),
            // Out-of-band options. All options except for the protocol version
            // and credentials are the defaults.
            protocol_version: bindings::ipmi_monitoring_protocol_version_IPMI_MONITORING_PROTOCOL_VERSION_2_0 as c_int,
            username,
            password,
            k_g: ptr::null_mut(),
            k_g_len: 0,
            privilege_level: -1,
            authentication_type: -1,
            cipher_suite_id: -1,
            session_timeout_len: 0,
            retransmission_timeout_len: 0,
            // Other options
            workaround_flags: 0,
        };

        Ok(Self { ctx, config, hostname })
    }

    fn error_msg(&self) -> Result<&'static str> {
        // [Unsafe] Similar to ipmi_monitoring_cts_strerror, this always returns
        // a valid string from a statically allocated region
        unsafe {
            from_static_utf8_cstr(
                bindings::ipmi_monitoring_ctx_errormsg(self.ctx))
        }
    }

    /// Set the directory for storing the SDR cache for the current host.
    pub fn set_sdr_cache_directory(&mut self, path: &Path) -> Result<()> {
        let cstr = path_to_cstring(path)?;

        // [Unsafe] String is valid and a copy of it will be saved in a
        // statically sized buffer within the context. If the path is too long,
        // an error is returned.
        let ret = unsafe {
            bindings::ipmi_monitoring_ctx_sdr_cache_directory(
                self.ctx, cstr.as_ptr())
        };
        if ret < 0 {
            return Err(Error::Lim {
                action: "set SDR cache directory",
                message: self.error_msg()?,
            });
        }

        Ok(())
    }

    /// Set the path to the config file containing sensor reading interpretation
    /// rules. If the path is [`None`], then the default sensor config file is
    /// used. If this function is never called, then only the interpretations
    /// built into libipmimonitoring are used.
    pub fn set_sensor_config_file(&mut self, path: Option<&Path>) -> Result<()> {
        // LIM does not store this string
        let path_cstr = match path {
            Some(p) => Some(path_to_cstring(p)?),
            None => None,
        };
        let path_ptr = path_cstr.as_ref()
            .map_or(ptr::null(), |s| s.as_ptr());

        // [Unsafe] NULL strings are valid
        let ret = unsafe {
            bindings::ipmi_monitoring_ctx_sensor_config_file(self.ctx, path_ptr)
        };
        if ret < 0 {
            return Err(Error::Lim {
                action: "set sensor config file",
                message: self.error_msg()?,
            });
        }

        Ok(())
    }

    /// Start iteration of temperature sensor readings. Use [`iterator_next`] to
    /// advance the iterator and [`read_sensor_name`]/[`read_sensor`] to get the
    /// actual values.
    pub fn temperature_sensor_readings(&mut self) -> Result<usize> {
        // LIM does not store this string
        let hostname_cstr = self.hostname.as_ref()
            .map(|s| CString::new(s.as_str()).unwrap());
        let hostname_ptr = hostname_cstr.as_ref()
            .map_or(ptr::null(), |s| s.as_ptr());
        let mut sensor_type = bindings::ipmi_monitoring_sensor_type_IPMI_MONITORING_SENSOR_TYPE_TEMPERATURE;

        // [Unsafe] config and sensor_type are passed as mutable pointers to
        // satisfy the type signature only. They are never modified. The
        // hostname pointer does not need to remain valid after the function
        // returns.
        let ret = unsafe {
            bindings::ipmi_monitoring_sensor_readings_by_sensor_type(
                self.ctx,
                hostname_ptr,
                ptr::addr_of_mut!(self.config),
                bindings::ipmi_monitoring_sensor_reading_flags_IPMI_MONITORING_SENSOR_READING_FLAGS_IGNORE_NON_INTERPRETABLE_SENSORS,
                ptr::addr_of_mut!(sensor_type),
                1,
                None,
                ptr::null_mut(),
            )
        };
        if ret < 0 {
            return Err(Error::Lim {
                action: "get temperature sensor readings",
                message: self.error_msg()?,
            });
        }

        Ok(ret as usize)
    }

    /// Advance to the next item when iterating through sensor readings.
    pub fn iterator_next(&mut self) -> Result<()> {
        // [Unsafe] No memory safety concerns
        let ret = unsafe {
            bindings::ipmi_monitoring_sensor_iterator_next(self.ctx)
        };
        if ret < 0 {
            return Err(Error::Lim {
                action: "get next sensor reading",
                message: self.error_msg()?,
            });
        }

        Ok(())
    }

    /// Get the sensor name for the current item during sensor reading
    /// iteration.
    pub fn read_sensor_name(&mut self) -> Result<String> {
        // [Unsafe] Returns non-owned pointer
        let ret = unsafe {
            bindings::ipmi_monitoring_sensor_read_sensor_name(self.ctx)
        };
        if ret.is_null() {
            return Err(Error::Lim {
                action: "read sensor name",
                message: self.error_msg()?,
            });
        }

        // [Unsafe] String is not NULL at this point
        let cstr = unsafe { CStr::from_ptr(ret) };

        Ok(cstr.to_str()?.to_owned())
    }

    /// Get the sensor reading (value and units) for the current item during
    /// sensor reading iteration.
    pub fn read_sensor(&mut self) -> Result<Option<SensorReading>> {
        // [Unsafe] No memory safety concerns
        let type_ret = unsafe {
            bindings::ipmi_monitoring_sensor_read_sensor_reading_type(self.ctx)
        };
        if type_ret < 0 {
            return Err(Error::Lim {
                action: "read sensor value type",
                message: self.error_msg()?,
            });
        }

        // [Unsafe] The return value can have multiple types and is determined
        // by type_ret
        let value_ret = unsafe {
            bindings::ipmi_monitoring_sensor_read_sensor_reading(self.ctx)
        };
        if value_ret.is_null() {
            return Ok(None);
        }

        // [Unsafe] Casting value_ret (void *) return value based on type_ret
        let value = unsafe {
            match type_ret as c_uint {
                bindings::ipmi_monitoring_sensor_reading_type_IPMI_MONITORING_SENSOR_READING_TYPE_UNSIGNED_INTEGER8_BOOL => {
                    SensorValue::Bool(*(value_ret as *const u8) != 0)
                }
                bindings::ipmi_monitoring_sensor_reading_type_IPMI_MONITORING_SENSOR_READING_TYPE_UNSIGNED_INTEGER32 => {
                    SensorValue::Uint32(*(value_ret as *const u32))
                }
                bindings::ipmi_monitoring_sensor_reading_type_IPMI_MONITORING_SENSOR_READING_TYPE_DOUBLE => {
                    SensorValue::Double(*(value_ret as *const f64))
                }
                _ => SensorValue::Unknown
            }
        };

        // [Unsafe] No memory safety concerns
        let units_ret = unsafe {
            bindings::ipmi_monitoring_sensor_read_sensor_units(self.ctx)
        };
        if units_ret < 0 {
            return Err(Error::Lim {
                action: "read sensor units",
                message: self.error_msg()?,
            });
        }

        let units = match units_ret as c_uint {
            bindings::ipmi_monitoring_sensor_units_IPMI_MONITORING_SENSOR_UNITS_CELSIUS =>
                SensorUnits::Celsius,
            bindings::ipmi_monitoring_sensor_units_IPMI_MONITORING_SENSOR_UNITS_FAHRENHEIT =>
                SensorUnits::Fahrenheit,
            o => SensorUnits::Unknown(o),
        };

        Ok(Some(SensorReading { value, units }))
    }
}

impl Drop for LimSession {
    fn drop(&mut self) {
        // [Unsafe] Context is always valid (non-NULL)
        unsafe { bindings::ipmi_monitoring_ctx_destroy(self.ctx) }

        if !self.config.username.is_null() {
            // [Unsafe] Allocated by CString::new() and never changed
            unsafe { CString::from_raw(self.config.username) };
        }
        if !self.config.password.is_null() {
            // [Unsafe] Allocated by CString::new() and never changed
            unsafe { CString::from_raw(self.config.password) };
        }
    }
}

/// The internal pointers and structs are safe to move between threads.
unsafe impl Send for LimSession {}
