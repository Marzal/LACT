#[macro_use]
mod macros;

pub use lact_schema as schema;

use anyhow::{anyhow, Context};
use nix::unistd::getuid;
use schema::{
    amdgpu_sysfs::gpu_handle::{power_profile_mode::PowerProfileModesTable, PerformanceLevel},
    request::{ConfirmCommand, SetClocksCommand},
    ClocksInfo, DeviceInfo, DeviceListEntry, DeviceStats, FanCurveMap, Request, Response,
    SystemInfo,
};
use serde::Deserialize;
use std::{
    io::{BufRead, BufReader, Write},
    marker::PhantomData,
    ops::DerefMut,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{error, info};

const RECONNECT_INTERVAL_MS: u64 = 250;

#[derive(Clone)]
pub struct DaemonClient {
    stream: Arc<Mutex<(BufReader<UnixStream>, UnixStream)>>,
    pub embedded: bool,
}

impl DaemonClient {
    pub fn connect() -> anyhow::Result<Self> {
        let path =
            get_socket_path().context("Could not connect to daemon: socket file not found")?;
        info!("connecting to service at {path:?}");
        let stream_pair = connect_pair(&path)?;

        Ok(Self {
            stream: Arc::new(Mutex::new(stream_pair)),
            embedded: false,
        })
    }

    pub fn from_stream(stream: UnixStream, embedded: bool) -> anyhow::Result<Self> {
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream: Arc::new(Mutex::new((reader, stream))),
            embedded,
        })
    }

    fn make_request<'a, T: Deserialize<'a>>(
        &self,
        request: Request,
    ) -> anyhow::Result<ResponseBuffer<T>> {
        let mut stream_guard = self.stream.lock().map_err(|err| anyhow!("{err}"))?;
        let (reader, writer) = stream_guard.deref_mut();

        if !reader.buffer().is_empty() {
            return Err(anyhow!("Another request was not processed properly"));
        }

        let response_payload = match process_request(&request, reader, writer) {
            Ok(payload) => payload,
            Err(err) => {
                error!("Could not make request: {err}, reconnecting to socket");
                let peer_addr = writer.peer_addr().context("Could not read peer address")?;
                let path = peer_addr
                    .as_pathname()
                    .context("Connected socket addr is not a path")?;

                loop {
                    match connect_pair(path) {
                        Ok(new_connection) => {
                            info!("Established new socket connection");
                            *stream_guard = new_connection;
                            drop(stream_guard);
                            return self.make_request(request);
                        }
                        Err(err) => {
                            error!("Could not reconnect: {err:#}, retrying in {RECONNECT_INTERVAL_MS}ms");
                            std::thread::sleep(Duration::from_millis(RECONNECT_INTERVAL_MS));
                        }
                    }
                }
            }
        };

        Ok(ResponseBuffer {
            buf: response_payload,
            _phantom: PhantomData,
        })
    }

    pub fn list_devices<'a>(&self) -> anyhow::Result<ResponseBuffer<Vec<DeviceListEntry<'a>>>> {
        self.make_request(Request::ListDevices)
    }

    pub fn set_fan_control(
        &self,
        id: &str,
        enabled: bool,
        curve: Option<FanCurveMap>,
    ) -> anyhow::Result<u64> {
        self.make_request(Request::SetFanControl { id, enabled, curve })?
            .inner()
    }

    pub fn set_power_cap(&self, id: &str, cap: Option<f64>) -> anyhow::Result<u64> {
        self.make_request(Request::SetPowerCap { id, cap })?.inner()
    }

    request_plain!(get_system_info, SystemInfo, SystemInfo);
    request_plain!(enable_overdrive, EnableOverdrive, ());
    request_with_id!(get_device_info, DeviceInfo, DeviceInfo);
    request_with_id!(get_device_stats, DeviceStats, DeviceStats);
    request_with_id!(get_device_clocks_info, DeviceClocksInfo, ClocksInfo);
    request_with_id!(
        get_device_power_profile_modes,
        DevicePowerProfileModes,
        PowerProfileModesTable
    );

    pub fn set_performance_level(
        &self,
        id: &str,
        performance_level: PerformanceLevel,
    ) -> anyhow::Result<u64> {
        self.make_request(Request::SetPerformanceLevel {
            id,
            performance_level,
        })?
        .inner()
    }

    pub fn set_clocks_value(&self, id: &str, command: SetClocksCommand) -> anyhow::Result<u64> {
        self.make_request(Request::SetClocksValue { id, command })?
            .inner()
    }

    pub fn batch_set_clocks_value(
        &self,
        id: &str,
        commands: Vec<SetClocksCommand>,
    ) -> anyhow::Result<u64> {
        self.make_request(Request::BatchSetClocksValue { id, commands })?
            .inner()
    }

    pub fn set_power_profile_mode(&self, id: &str, index: Option<u16>) -> anyhow::Result<u64> {
        self.make_request(Request::SetPowerProfileMode { id, index })?
            .inner()
    }

    pub fn confirm_pending_config(&self, command: ConfirmCommand) -> anyhow::Result<()> {
        self.make_request(Request::ConfirmPendingConfig(command))?
            .inner()
    }
}

fn get_socket_path() -> Option<PathBuf> {
    let root_path = PathBuf::from("/var/run/lactd.sock");

    if root_path.exists() {
        return Some(root_path);
    }

    let uid = getuid();
    let user_path = PathBuf::from(format!("/var/run/user/{}/lactd.sock", uid));

    if user_path.exists() {
        Some(user_path)
    } else {
        None
    }
}

pub struct ResponseBuffer<T> {
    buf: String,
    _phantom: PhantomData<T>,
}

impl<'a, T: Deserialize<'a>> ResponseBuffer<T> {
    pub fn inner(&'a self) -> anyhow::Result<T> {
        let response: Response<T> = serde_json::from_str(&self.buf)
            .context("Could not deserialize response from daemon")?;
        match response {
            Response::Ok(data) => Ok(data),
            Response::Error(err) => Err(anyhow!("Got error from daemon: {err}")),
        }
    }
}

fn connect_pair(path: &Path) -> anyhow::Result<(BufReader<UnixStream>, UnixStream)> {
    let stream = UnixStream::connect(path).context("Could not connect to daemon")?;
    let reader = BufReader::new(stream.try_clone()?);
    Ok((reader, stream))
}

fn process_request(
    request: &Request,
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
) -> anyhow::Result<String> {
    let request_payload = serde_json::to_string(request)?;
    writer.write_all(request_payload.as_bytes())?;
    writer.write_all(b"\n")?;

    let mut response_payload = String::new();
    reader.read_line(&mut response_payload)?;

    Ok(response_payload)
}
