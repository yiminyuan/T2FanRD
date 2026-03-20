#![warn(rust_2018_idioms)]
#![warn(clippy::pedantic)]
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::module_name_repetitions
)]

use std::{
    io::ErrorKind,
    path::PathBuf,
    process::ExitCode,
    sync::{atomic::AtomicBool, Arc},
};

use arraydeque::ArrayDeque;
use fan_controller::{read_temp_file, FanController};
use nonempty::NonEmpty as NonEmptyVec;
use signal_hook::consts::{SIGINT, SIGTERM};

use config::load_fan_configs;
use error::{Error, Result};

mod config;
mod error;
mod fan_controller;

#[cfg(not(target_os = "linux"))]
compile_error!("This tool is only developed for Linux systems.");

#[cfg(debug_assertions)]
const PID_FILE: &str = "t2fand.pid";
#[cfg(not(debug_assertions))]
const PID_FILE: &str = "/run/t2fand.pid";

fn get_current_euid() -> libc::uid_t {
    // SAFETY: FFI call with no preconditions
    unsafe { libc::geteuid() }
}

fn find_fan_paths() -> Result<NonEmptyVec<PathBuf>> {
    // APP0001:00/fan1_label
    let fan = glob::glob("/sys/devices/pci*/*/*/*/APP0001:00/fan*")?
        .filter_map(Result::ok)
        .find(|p| p.exists())
        .ok_or(Error::NoFan)?;

    // APP0001:00
    let first_fan_path = fan.parent().ok_or(Error::NoFan)?;
    // APP0001:00/fan*_input
    let fan_glob = first_fan_path.display().to_string() + "/fan*_input";
    // APP0001:00/fan1
    let fans = glob::glob(&fan_glob)?
        .filter_map(Result::ok)
        .filter_map(|mut path| {
            let file_name = path.file_name()?.to_str()?;
            let fan_name = file_name.strip_suffix("_input")?;
            let fan_name_owned = fan_name.to_owned();
            path.set_file_name(fan_name_owned);
            Some(path)
        });

    NonEmptyVec::collect(fans).ok_or(Error::NoFan)
}

fn check_pid_file() -> Result<()> {
    match std::fs::read_to_string(PID_FILE) {
        Ok(pid) => {
            let mut proc_path = std::path::PathBuf::new();
            proc_path.push("/proc");
            proc_path.push(pid);

            if proc_path.exists() {
                return Err(Error::AlreadyRunning);
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(Error::PidRead(err)),
    };

    let current_pid = std::process::id().to_string();
    std::fs::write(PID_FILE, current_pid).map_err(Error::PidWrite)
}

fn find_cpu_temp_file(temp_buf: &mut String) -> Result<std::fs::File> {
    let temps = glob::glob("/sys/devices/platform/coretemp.0/hwmon/hwmon*/temp1_input")?;
    for temp_path_res in temps {
        let Ok(temp_path) = temp_path_res else {
            eprintln!("Unable to read glob path");
            continue;
        };

        let Ok(mut temp_file) = std::fs::File::open(temp_path) else {
            eprintln!("Unable to open temperature sensor");
            continue;
        };

        if read_temp_file(&mut temp_file, temp_buf).is_ok() {
            return Ok(temp_file);
        }
    }

    Err(Error::NoCpu)
}

/// Read all PCI addresses associated with a physical PCIe slot and its
/// sub-slots from `/sys/bus/pci/slots/`.
///
/// For slot `"1"`, this reads addresses from slots named `1`, `1-1`, `1-2`,
/// etc. The addresses are in `DDDD:BB:DD` format (no function number).
fn read_slot_addresses(slot: &str) -> Result<Vec<String>> {
    let mut addresses = Vec::new();

    let main_addr_path = format!("/sys/bus/pci/slots/{slot}/address");
    match std::fs::read_to_string(&main_addr_path) {
        Ok(addr) => addresses.push(addr.trim().to_owned()),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(Error::SensorNotFound(format!("slot:{slot}")));
        }
        Err(err) => return Err(Error::TempRead(err)),
    }

    // Also read sub-slot addresses (slot-1, slot-2, etc.)
    let pattern = format!("/sys/bus/pci/slots/{slot}-*/address");
    for path in glob::glob(&pattern)?.filter_map(std::result::Result::ok) {
        if let Ok(addr) = std::fs::read_to_string(&path) {
            addresses.push(addr.trim().to_owned());
        }
    }

    Ok(addresses)
}

/// Find all hwmon temp1_input files for devices downstream of a physical PCIe
/// slot. The slot number corresponds to entries in `/sys/bus/pci/slots/`.
///
/// This matches by checking whether the hwmon device's canonical sysfs path
/// passes through any PCI address owned by the slot or its sub-slots.
fn find_slot_temp_files(slot: &str) -> Result<Vec<std::fs::File>> {
    let addresses = read_slot_addresses(slot)?;
    let mut files = Vec::new();

    let hwmon_paths = glob::glob("/sys/class/hwmon/hwmon*")?;
    for hwmon_path in hwmon_paths.filter_map(std::result::Result::ok) {
        let temp_input = hwmon_path.join("temp1_input");
        if !temp_input.exists() {
            continue;
        }

        let device_path = hwmon_path.join("device");
        let Ok(real_path) = std::fs::canonicalize(&device_path) else {
            continue;
        };

        let real_path_str = real_path.to_string_lossy();

        // Check if any path component matches a slot address.
        // Slot addresses are "DDDD:BB:DD" (no function), sysfs path components
        // are "DDDD:BB:DD.F" (with function), so we match the component prefix.
        let is_downstream = addresses.iter().any(|addr| {
            real_path_str.split('/').any(|component| {
                component.starts_with(addr.as_str())
                    && component
                        .as_bytes()
                        .get(addr.len())
                        .map_or(true, |&b| b == b'.')
            })
        });

        if is_downstream {
            if let Ok(file) = std::fs::File::open(&temp_input) {
                files.push(file);
            }
        }
    }

    if files.is_empty() {
        return Err(Error::SensorNotFound(format!("slot:{slot}")));
    }

    Ok(files)
}

/// Resolve a list of sensor specifiers to open file handles for their
/// temp1_input files.
///
/// Each specifier must be in `slot:<N>` format, referring to a physical PCIe
/// slot number as exposed in `/sys/bus/pci/slots/`.
fn find_sensor_temp_files(sensor_names: &[String]) -> Result<Vec<std::fs::File>> {
    let mut files = Vec::new();
    for name in sensor_names {
        if let Some(slot) = name.strip_prefix("slot:") {
            files.extend(find_slot_temp_files(slot)?);
        } else {
            return Err(Error::InvalidConfigValue("sensors (expected slot:<N> format)"));
        }
    }
    Ok(files)
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

struct FanTempTracker {
    temps: ArrayDeque<u8, 50, arraydeque::Wrapping>,
    last_mean: u16,
}

fn start_temp_loop(
    mut temp_buffer: String,
    mut cpu_temp_file: std::fs::File,
    fans: &mut NonEmptyVec<FanController>,
) -> Result<()> {
    let cancellation_token = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, cancellation_token.clone()).map_err(Error::Signal)?;
    signal_hook::flag::register(SIGTERM, cancellation_token.clone()).map_err(Error::Signal)?;

    let mut trackers: Vec<FanTempTracker> = fans
        .iter()
        .map(|_| FanTempTracker {
            temps: ArrayDeque::new(),
            last_mean: 0,
        })
        .collect();

    while !cancellation_token.load(std::sync::atomic::Ordering::Relaxed) {
        let cpu_temp = read_temp_file(&mut cpu_temp_file, &mut temp_buffer)?;

        let mut any_changed = false;
        for (fan, tracker) in fans.iter_mut().zip(trackers.iter_mut()) {
            let fan_temp = if let Some(sensor_temp) = fan.read_sensor_temp(&mut temp_buffer)? {
                sensor_temp
            } else {
                cpu_temp
            };

            tracker.temps.push_back(fan_temp);

            let sum_temp: u16 = tracker.temps.iter().map(|t| *t as u16).sum();
            let mean_temp = sum_temp / (tracker.temps.len() as u16);

            if mean_temp != tracker.last_mean {
                tracker.last_mean = mean_temp;
                fan.set_speed(fan.calc_speed(mean_temp as u8))?;
                any_changed = true;
            } else {
                // Avoid messing up the mean due to the longer sleep.
                for _ in 0..9 {
                    tracker.temps.push_back(fan_temp);
                }
            }
        }

        if any_changed {
            std::thread::sleep(std::time::Duration::from_millis(100));
        } else {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    Ok(())
}

fn real_main() -> Result<()> {
    if get_current_euid() != 0 {
        return Err(Error::NotRoot);
    }

    check_pid_file()?;

    let mut temp_buffer = String::new();

    let fan_paths = find_fan_paths()?;
    let fan_count = fan_paths.len_nonzero();
    let configs = load_fan_configs(fan_count)?;

    let fans: Vec<FanController> = fan_paths
        .into_iter()
        .zip(configs)
        .map(|(path, config)| {
            let sensor_files = find_sensor_temp_files(&config.sensors)?;
            FanController::new(path, config, sensor_files)
        })
        .collect::<Result<_>>()?;

    let mut fans = NonEmptyVec::from_vec(fans).ok_or(Error::NoFan)?;

    let cpu_temp_file = find_cpu_temp_file(&mut temp_buffer)?;

    println!();
    for fan in &fans {
        fan.set_manual(true)?;
    }

    let res = start_temp_loop(temp_buffer, cpu_temp_file, &mut fans);
    println!("T2 Fan Daemon is shutting down...");
    for fan in &fans {
        fan.set_manual(false)?;
    }

    let pid_res = std::fs::remove_file(PID_FILE).map_err(Error::PidDelete);
    match (res, pid_res) {
        (Err(err), _) | (_, Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}
