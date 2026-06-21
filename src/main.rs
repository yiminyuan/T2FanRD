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
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{atomic::AtomicBool, Arc},
};

use fan_controller::{read_temp_file, FanController, SensorIdx, SensorPool};
use nonempty::NonEmpty as NonEmptyVec;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use config::{load_fan_configs, FanConfig, SensorSpec};
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

const SPIKE_THRESHOLD_C: u8 = 5;

// EMA decay coefficients chosen to give a ~5 s effective time constant in
// both sleep cadences. With the short (100 ms) tick, alpha = 1 - exp(-100/5000)
// ≈ 0.02; with the long (1 s) tick, alpha = 1 - exp(-1000/5000) ≈ 0.18.
const ALPHA_SHORT: f32 = 0.02;
const ALPHA_LONG: f32 = 0.18;

fn get_current_euid() -> libc::uid_t {
    // SAFETY: FFI call with no preconditions
    unsafe { libc::geteuid() }
}

/// Locate the `macsmc_hwmon` hwmon directory (the T2 SMC fan controller on
/// kernel 7.1+). The hwmon index is not stable across boots, so match by the
/// `name` attribute rather than a fixed path.
fn find_macsmc_hwmon() -> Result<PathBuf> {
    for name_path in glob::glob("/sys/class/hwmon/hwmon*/name")?.filter_map(Result::ok) {
        if let Ok(name) = std::fs::read_to_string(&name_path) {
            if name.trim() == "macsmc_hwmon" {
                return name_path
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or(Error::NoFan);
            }
        }
    }
    Err(Error::NoFan)
}

/// Discover fan control stems under the macsmc hwmon, e.g.
/// `/sys/class/hwmon/hwmonN/fan1`. Each stem suffixes to `_min` / `_max` /
/// `_target` / `_input`. Returned in ascending fan index so the order lines
/// up with the `[Fan1]`, `[Fan2]`, … config sections.
fn find_fan_paths() -> Result<NonEmptyVec<PathBuf>> {
    let hwmon_dir = find_macsmc_hwmon()?;
    let fan_glob = hwmon_dir.join("fan*_input");
    let fan_glob = fan_glob.to_str().ok_or(Error::NoFan)?;

    let mut fans: Vec<(u32, PathBuf)> = glob::glob(fan_glob)?
        .filter_map(Result::ok)
        .filter_map(|mut path| {
            let file_name = path.file_name()?.to_str()?;
            let stem = file_name.strip_suffix("_input")?;
            let index: u32 = stem.strip_prefix("fan")?.parse().ok()?;
            let stem = stem.to_owned();
            path.set_file_name(stem);
            Some((index, path))
        })
        .collect();

    fans.sort_by_key(|(index, _)| *index);
    let fan_paths: Vec<PathBuf> = fans.into_iter().map(|(_, path)| path).collect();

    NonEmptyVec::from_vec(fan_paths).ok_or(Error::NoFan)
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
    }

    let current_pid = std::process::id().to_string();
    std::fs::write(PID_FILE, current_pid).map_err(Error::PidWrite)
}

fn find_cpu_temp_file() -> Result<std::fs::File> {
    let temps = glob::glob("/sys/devices/platform/coretemp.0/hwmon/hwmon*/temp1_input")?;
    for temp_path_res in temps {
        let Ok(temp_path) = temp_path_res else {
            eprintln!("Unable to read glob path");
            continue;
        };

        let Ok(temp_file) = std::fs::File::open(temp_path) else {
            eprintln!("Unable to open temperature sensor");
            continue;
        };

        if read_temp_file(&temp_file).is_ok() {
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

/// True if the PCI device at `device_path` has class `0x03` (display
/// controller). Used to filter slot hwmons down to GPU sensors and exclude
/// incidental devices (ethernet, audio, NVMe) that happen to share a slot's
/// PCIe sub-tree.
fn is_display_controller(device_path: &Path) -> bool {
    std::fs::read_to_string(device_path.join("class"))
        .ok()
        .and_then(|s| {
            let hex = s.trim().strip_prefix("0x")?;
            u8::from_str_radix(hex.get(..2)?, 16).ok()
        })
        .is_some_and(|class_byte| class_byte == 0x03)
}

/// Scan every GPU hwmon and attribute it to a single slot — the slot whose
/// matching address appears closest to the PCI root in the device's canonical
/// path. Disambiguates cards cross-connected via Infinity Fabric Link (whose
/// dies show up downstream of multiple slots' sub-slot address lists).
fn resolve_slot_attribution(slot_ids: &[String]) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut slot_addresses: HashMap<String, Vec<String>> = HashMap::new();
    for slot in slot_ids {
        slot_addresses.insert(slot.clone(), read_slot_addresses(slot)?);
    }

    let mut slot_hwmons: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for hwmon_path in glob::glob("/sys/class/hwmon/hwmon*")?.filter_map(std::result::Result::ok) {
        let temp_input = hwmon_path.join("temp1_input");
        if !temp_input.exists() {
            continue;
        }

        let device_path = hwmon_path.join("device");
        let Ok(real_path) = std::fs::canonicalize(&device_path) else {
            continue;
        };

        if !is_display_controller(&real_path) {
            continue;
        }

        let real_path_str = real_path.to_string_lossy();
        let components: Vec<&str> = real_path_str.split('/').collect();

        // For each configured slot, find the earliest depth at which any of
        // its addresses matches a component (with the `.`-or-end boundary
        // check so 0000:0b:00 doesn't match 0000:0b:001). The slot whose
        // match is closest to the PCI root wins — that's the physical owner.
        let mut best: Option<(&String, usize)> = None;
        for (slot_id, addresses) in &slot_addresses {
            let depth = components.iter().enumerate().find_map(|(d, c)| {
                addresses
                    .iter()
                    .any(|addr| {
                        c.starts_with(addr.as_str())
                            && c.as_bytes().get(addr.len()).is_none_or(|&b| b == b'.')
                    })
                    .then_some(d)
            });

            if let Some(d) = depth {
                if best.is_none_or(|(_, best_d)| d < best_d) {
                    best = Some((slot_id, d));
                }
            }
        }

        if let Some((slot_id, _)) = best {
            slot_hwmons
                .entry(slot_id.clone())
                .or_default()
                .push(temp_input);
        }
    }

    Ok(slot_hwmons)
}

/// Build the daemon-level `SensorPool` (each unique sensor opened once) plus
/// per-fan `Vec<SensorIdx>` lists referencing the pool. Fans that share the
/// same underlying hwmon path get the same index — read once per tick,
/// regardless of how many fans need the value.
fn build_sensor_setup(
    tracked_specs: &[Vec<SensorSpec>],
    slot_hwmons: &HashMap<String, Vec<PathBuf>>,
) -> Result<(SensorPool, Vec<Vec<SensorIdx>>)> {
    let needs_cpu = tracked_specs
        .iter()
        .flatten()
        .any(|s| matches!(s, SensorSpec::Cpu));
    let cpu_file = if needs_cpu {
        Some(find_cpu_temp_file()?)
    } else {
        None
    };

    let mut hwmons: Vec<std::fs::File> = Vec::new();
    let mut path_to_idx: HashMap<PathBuf, usize> = HashMap::new();
    let mut fan_idx_lists: Vec<Vec<SensorIdx>> = Vec::with_capacity(tracked_specs.len());

    for specs in tracked_specs {
        let mut idx_list = Vec::new();
        for spec in specs {
            match spec {
                SensorSpec::Cpu => idx_list.push(SensorIdx::Cpu),
                SensorSpec::Slot(n) => {
                    let paths = slot_hwmons
                        .get(n)
                        .ok_or_else(|| Error::SensorNotFound(format!("slot:{n}")))?;
                    for path in paths {
                        let idx = if let Some(&i) = path_to_idx.get(path) {
                            i
                        } else {
                            let file = std::fs::File::open(path).map_err(Error::FanOpen)?;
                            hwmons.push(file);
                            let i = hwmons.len() - 1;
                            path_to_idx.insert(path.clone(), i);
                            i
                        };
                        idx_list.push(SensorIdx::Hwmon(idx));
                    }
                }
            }
        }
        fan_idx_lists.push(idx_list);
    }

    Ok((SensorPool { cpu_file, hwmons }, fan_idx_lists))
}

fn release_fan_to_smc(fan_path: &Path) -> Result<()> {
    let file_name = fan_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(Error::NoFan)?;
    let target_path = fan_path.with_file_name(format!("{file_name}_target"));
    std::fs::write(&target_path, b"0").map_err(Error::FanWrite)
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
    ema: Option<f32>,
    last_pwm: u32,
}

fn start_temp_loop(
    sensor_pool: &SensorPool,
    fans: &mut NonEmptyVec<FanController>,
    auto_pattern: &[bool],
    fan_count: std::num::NonZeroUsize,
) -> Result<()> {
    let cancellation_token = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, cancellation_token.clone()).map_err(Error::Signal)?;
    signal_hook::flag::register(SIGTERM, cancellation_token.clone()).map_err(Error::Signal)?;

    let reload_token = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGHUP, reload_token.clone()).map_err(Error::Signal)?;

    let mut trackers: Vec<FanTempTracker> = fans
        .iter()
        .map(|_| FanTempTracker {
            ema: None,
            last_pwm: 0,
        })
        .collect();

    let mut was_long_sleep = false;
    while !cancellation_token.load(std::sync::atomic::Ordering::Relaxed) {
        if reload_token.swap(false, std::sync::atomic::Ordering::Relaxed) {
            match try_reload_curves(fans, auto_pattern, fan_count) {
                Ok(()) => println!("Config reloaded."),
                Err(e) => eprintln!("Config reload failed, keeping current config: {e}"),
            }
        }

        let readings = sensor_pool.read_all()?;

        let mut any_changed = false;
        for (fan, tracker) in fans.iter_mut().zip(trackers.iter_mut()) {
            let fan_temp = fan.compute_max_temp(&readings);

            // EMA update; alpha scales with sleep cadence so wall-clock
            // smoothing is constant regardless of which branch ran.
            let alpha = if was_long_sleep { ALPHA_LONG } else { ALPHA_SHORT };
            let prev_ema_int = tracker.ema.map_or(fan_temp, |v| v as u8);
            let new_ema = match tracker.ema {
                Some(prev) => alpha * fan_temp as f32 + (1.0 - alpha) * prev,
                None => fan_temp as f32,
            };
            tracker.ema = Some(new_ema);

            // Asymmetric ramp: a rising edge bypasses the smoothed value so
            // we react in one tick. Falling temps continue using the EMA.
            let effective_temp = if fan_temp > prev_ema_int.saturating_add(SPIKE_THRESHOLD_C) {
                fan_temp
            } else {
                new_ema as u8
            };

            let new_pwm = fan.calc_speed(effective_temp);
            let pwm_threshold = ((fan.max_speed() - fan.min_speed()) / 100).max(5);
            if new_pwm.abs_diff(tracker.last_pwm) >= pwm_threshold {
                tracker.last_pwm = new_pwm;
                fan.set_speed(new_pwm)?;
                any_changed = true;
            }
        }

        if any_changed {
            std::thread::sleep(std::time::Duration::from_millis(100));
            was_long_sleep = false;
        } else {
            std::thread::sleep(std::time::Duration::from_secs(1));
            was_long_sleep = true;
        }
    }

    Ok(())
}

/// Re-parse `t2fand.conf` and apply curve parameter changes to each tracked
/// fan in place. Rejects with `ConfigStructureChanged` if `auto` toggled at
/// any position or `sensors` changed for any tracked fan — those require a
/// daemon restart. On any error, leaves the running state untouched.
fn try_reload_curves(
    fans: &mut NonEmptyVec<FanController>,
    auto_pattern: &[bool],
    fan_count: std::num::NonZeroUsize,
) -> Result<()> {
    let new_configs = load_fan_configs(fan_count)?;

    if new_configs.len() != auto_pattern.len() {
        return Err(Error::ConfigStructureChanged(
            "fan count differs".to_owned(),
        ));
    }
    for (i, (new_config, was_auto)) in new_configs.iter().zip(auto_pattern).enumerate() {
        if new_config.auto != *was_auto {
            return Err(Error::ConfigStructureChanged(format!(
                "auto flag changed at Fan{}",
                i + 1
            )));
        }
    }

    let new_tracked: Vec<&FanConfig> = new_configs.iter().filter(|c| !c.auto).collect();
    if new_tracked.len() != fans.len() {
        return Err(Error::ConfigStructureChanged(
            "tracked fan count differs".to_owned(),
        ));
    }

    for (i, (fan, new_config)) in fans.iter().zip(&new_tracked).enumerate() {
        if fan.config().sensors != new_config.sensors {
            return Err(Error::ConfigStructureChanged(format!(
                "sensors changed at tracked fan #{}",
                i + 1
            )));
        }
    }

    for (fan, new_config) in fans.iter_mut().zip(new_tracked) {
        fan.set_config(new_config.clone());
    }

    Ok(())
}

fn real_main() -> Result<()> {
    if get_current_euid() != 0 {
        return Err(Error::NotRoot);
    }

    check_pid_file()?;

    let fan_paths = find_fan_paths()?;
    let fan_count = fan_paths.len_nonzero();
    let configs = load_fan_configs(fan_count)?;
    let auto_pattern: Vec<bool> = configs.iter().map(|c| c.auto).collect();

    let mut auto_paths: Vec<PathBuf> = Vec::new();
    let mut tracked: Vec<(PathBuf, FanConfig)> = Vec::new();
    for (path, config) in fan_paths.into_iter().zip(configs) {
        if config.auto {
            auto_paths.push(path);
        } else {
            tracked.push((path, config));
        }
    }

    // Force any auto=true fan back to SMC mode in case a previous daemon run
    // left it in manual. Best-effort: if fan_control is off the fan is
    // already on the SMC curve (manual was never possible), so a failed
    // write here is benign. Daemon then ignores these fans.
    for path in &auto_paths {
        if let Err(e) = release_fan_to_smc(path) {
            eprintln!("Failed to release auto fan to SMC: {e}");
        }
    }

    if tracked.is_empty() {
        return Err(Error::AllFansAuto);
    }

    let unique_slots: Vec<String> = {
        let mut set = HashSet::new();
        for (_, config) in &tracked {
            for spec in &config.sensors {
                if let SensorSpec::Slot(n) = spec {
                    set.insert(n.clone());
                }
            }
        }
        set.into_iter().collect()
    };
    let slot_hwmons = resolve_slot_attribution(&unique_slots)?;

    let tracked_specs: Vec<Vec<SensorSpec>> =
        tracked.iter().map(|(_, c)| c.sensors.clone()).collect();
    let (sensor_pool, fan_idx_lists) = build_sensor_setup(&tracked_specs, &slot_hwmons)?;
    println!("Sensor pool: {sensor_pool:#?}");

    let fans: Vec<FanController> = tracked
        .into_iter()
        .zip(fan_idx_lists)
        .map(|((path, config), sensors)| FanController::new(path, config, sensors))
        .collect::<Result<_>>()?;

    let mut fans = NonEmptyVec::from_vec(fans).ok_or(Error::NoFan)?;

    println!();
    // No explicit manual-engage: macsmc switches a fan to manual on the first
    // fanN_target write, and the loop's first tick always writes (last_pwm
    // starts at 0), so the fan is taken over on tick 1.

    let res = start_temp_loop(&sensor_pool, &mut fans, &auto_pattern, fan_count);

    // Release every tracked fan to SMC auto on exit (stop, restart, reboot,
    // shutdown). This is the maximal handoff the SMC allows; it does not fully
    // reset the SMC's auto curve (see README / CLAUDE.md), but it is the right
    // thing on every exit path. The service's ExecStopPost repeats this as a
    // best-effort net in case the daemon is SIGKILLed before reaching here.
    println!("T2 Fan Daemon is shutting down, releasing fans to SMC auto...");
    for fan in &fans {
        if let Err(e) = fan.release_to_auto() {
            eprintln!("Failed to release fan to SMC auto: {e}");
        }
    }

    let pid_res = std::fs::remove_file(PID_FILE).map_err(Error::PidDelete);
    match (res, pid_res) {
        (Err(err), _) | (_, Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}
