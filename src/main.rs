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

fn find_temp_file(temps: glob::Paths, temp_buf: &mut String) -> Option<std::fs::File> {
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
            return Some(temp_file);
        }
    }

    None
}

fn find_cpu_temp_file(temp_buf: &mut String) -> Result<std::fs::File> {
    let temps = glob::glob("/sys/devices/platform/coretemp.0/hwmon/hwmon*/temp1_input")?;
    find_temp_file(temps, temp_buf).ok_or(Error::NoCpu)
}

fn find_gpu_temp_file(temp_buf: &mut String) -> Result<Option<std::fs::File>> {
    // The Mac Pro 2019 (MacPro7,1) has no integrated GPU; its discrete GPUs
    // are handled via the per-fan `sensors` config instead.
    if let Ok(product_name) = std::fs::read_to_string("/sys/class/dmi/id/product_name") {
        if product_name.trim() == "MacPro7,1" {
            println!("MacPro7,1 detected, skipping integrated GPU temperature sensor");
            return Ok(None);
        }
    }

    let temps = glob::glob("/sys/class/drm/card0/device/hwmon/hwmon*/temp1_input")?;
    Ok(find_temp_file(temps, temp_buf))
}

/// Construct an lm_sensors-compatible chip name from a hwmon driver name and
/// the canonicalized device path.
///
/// For a PCI device at `/sys/devices/pci0000:00/.../0000:0b:00.0`, with driver
/// name `amdgpu`, this returns `amdgpu-pci-0b00`.
fn construct_chip_name(driver_name: &str, device_path: &std::path::Path) -> Option<String> {
    let device_name = device_path.file_name()?.to_str()?;
    // Parse PCI address: DDDD:BB:DD.F
    let parts: Vec<&str> = device_name.split(':').collect();
    if parts.len() == 3 {
        let bus = parts[1]; // e.g. "0b"
        let dev_func = parts[2]; // e.g. "00.0"
        let dev = dev_func.split('.').next()?; // e.g. "00"
        Some(format!("{driver_name}-pci-{bus}{dev}"))
    } else {
        None
    }
}

/// Find the hwmon temp1_input file for a given lm_sensors-style sensor name
/// (e.g. `amdgpu-pci-0b00`).
fn find_hwmon_sensor(sensor_name: &str) -> Result<std::fs::File> {
    let hwmon_paths = glob::glob("/sys/class/hwmon/hwmon*")?;
    for hwmon_path in hwmon_paths.filter_map(std::result::Result::ok) {
        let name_path = hwmon_path.join("name");
        let Ok(name) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        let name = name.trim();

        let device_path = hwmon_path.join("device");
        if let Ok(real_path) = std::fs::canonicalize(&device_path) {
            if let Some(chip_name) = construct_chip_name(name, &real_path) {
                if chip_name == sensor_name {
                    let temp_input = hwmon_path.join("temp1_input");
                    return std::fs::File::open(temp_input).map_err(Error::TempRead);
                }
            }
        }
    }

    Err(Error::SensorNotFound(sensor_name.to_owned()))
}

/// Resolve a list of sensor names to open file handles for their temp1_input files.
fn find_sensor_temp_files(sensor_names: &[String]) -> Result<Vec<std::fs::File>> {
    sensor_names.iter().map(|name| find_hwmon_sensor(name)).collect()
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
    mut gpu_temp_file: Option<std::fs::File>,
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
        let default_temp = if let Some(gpu_temp_file) = &mut gpu_temp_file {
            let gpu_temp = read_temp_file(gpu_temp_file, &mut temp_buffer)?;
            gpu_temp.max(cpu_temp)
        } else {
            cpu_temp
        };

        let mut any_changed = false;
        for (fan, tracker) in fans.iter_mut().zip(trackers.iter_mut()) {
            let fan_temp = if let Some(sensor_temp) = fan.read_sensor_temp(&mut temp_buffer)? {
                sensor_temp
            } else {
                default_temp
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
    let gpu_temp_file = find_gpu_temp_file(&mut temp_buffer)?;

    println!();
    for fan in &fans {
        fan.set_manual(true)?;
    }

    let res = start_temp_loop(temp_buffer, cpu_temp_file, gpu_temp_file, &mut fans);
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
