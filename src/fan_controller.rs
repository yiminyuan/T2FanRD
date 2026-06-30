use std::{
    io::{IsTerminal, Write},
    os::unix::fs::FileExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};

use crate::{
    config::{FanConfig, SpeedCurve},
    error::{Error, Result},
};

// NVIDIA GPUs cool themselves with their own VBIOS-controlled fan; the case
// fans we drive only supply fresh intake air (a slow ambient-assist role). So
// we sample NVML at a low rate and reuse the cached value between reads,
// keeping the reactive 10 Hz loop (for the fanless CPU / AMD GPUs) free of
// NVML ioctls.
const NVML_MIN_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn read_temp_file(temp_file: &std::fs::File) -> Result<u8> {
    let mut buf = [0u8; 16];
    let n = temp_file.read_at(&mut buf, 0).map_err(Error::TempRead)?;
    let s = std::str::from_utf8(&buf[..n]).map_err(|_| Error::TempUtf8)?;
    let temp = s.trim_end().parse::<u32>().map_err(Error::TempParse)?;
    Ok((temp / 1000) as u8)
}

#[derive(Debug)]
pub enum SensorIdx {
    Cpu,
    Hwmon(usize),
    Nvidia(usize),
}

/// NVML handle plus the unique GPU indices to sample. Reads are throttled to
/// `NVML_MIN_INTERVAL` and cached: the case fan's effect on GPU intake air is
/// slow, the GPU runs its own fan, and there is no reason to stress the
/// NVIDIA driver at the control loop's 10 Hz.
pub struct NvidiaSensors {
    nvml: Nvml,
    indices: Vec<u32>,
    cache: Vec<u8>,
    last_read: Option<Instant>,
}

// `Nvml` is not `Debug`; show only the GPU indices so `SensorPool` can derive
// `Debug` (the startup pool dump).
impl std::fmt::Debug for NvidiaSensors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvidiaSensors")
            .field("indices", &self.indices)
            .finish_non_exhaustive()
    }
}

impl NvidiaSensors {
    pub fn new(nvml: Nvml, indices: Vec<u32>) -> Self {
        let cache = vec![0u8; indices.len()];
        Self {
            nvml,
            indices,
            cache,
            last_read: None,
        }
    }

    /// Refresh the cached GPU temperatures if the throttle interval has elapsed
    /// (or on the first call), then hand back the cache.
    fn read(&mut self) -> Result<&[u8]> {
        let now = Instant::now();
        let due = self
            .last_read
            .is_none_or(|t| now.duration_since(t) >= NVML_MIN_INTERVAL);
        if due {
            for (slot, &index) in self.cache.iter_mut().zip(&self.indices) {
                let device = self.nvml.device_by_index(index).map_err(Error::NvmlRead)?;
                let temp = device
                    .temperature(TemperatureSensor::Gpu)
                    .map_err(Error::NvmlRead)?;
                *slot = temp.min(u32::from(u8::MAX)) as u8;
            }
            self.last_read = Some(now);
        }
        Ok(&self.cache)
    }
}

/// Daemon-level pool of unique sensor handles. Each unique CPU / hwmon
/// `temp1_input` is opened once and read once per tick; NVIDIA GPUs are read
/// through a single shared NVML handle, throttled. Regardless of how many fans
/// reference a sensor, it is read once per tick.
#[derive(Debug)]
pub struct SensorPool {
    pub cpu_file: Option<std::fs::File>,
    pub hwmons: Vec<std::fs::File>,
    pub nvidia: Option<NvidiaSensors>,
}

#[derive(Debug)]
pub struct SensorReadings {
    pub cpu: Option<u8>,
    pub hwmons: Vec<u8>,
    pub nvidia: Vec<u8>,
}

impl SensorPool {
    pub fn read_all(&mut self) -> Result<SensorReadings> {
        let cpu = match &self.cpu_file {
            Some(f) => Some(read_temp_file(f)?),
            None => None,
        };
        let mut hwmons = Vec::with_capacity(self.hwmons.len());
        for f in &self.hwmons {
            hwmons.push(read_temp_file(f)?);
        }
        let nvidia = match &mut self.nvidia {
            Some(nv) => nv.read()?.to_vec(),
            None => Vec::new(),
        };
        Ok(SensorReadings {
            cpu,
            hwmons,
            nvidia,
        })
    }
}

#[derive(Debug)]
pub struct FanController {
    manual_file: std::fs::File,
    output_file: std::fs::File,
    config: FanConfig,

    min_speed: u32,
    max_speed: u32,
    sensors: Vec<SensorIdx>,
}

impl FanController {
    pub fn new(path: PathBuf, config: FanConfig, sensors: Vec<SensorIdx>) -> Result<Self> {
        fn join_suffix(mut path: PathBuf, suffix: &str) -> PathBuf {
            let file_name = path.file_name().unwrap().to_str().unwrap();
            path.set_file_name(format!("{file_name}{suffix}"));
            path
        }

        let min_speed = std::fs::read_to_string(join_suffix(path.clone(), "_min"))
            .map_err(Error::MinSpeedRead)?
            .trim()
            .parse()
            .map_err(Error::MinSpeedParse)?;

        let max_speed = std::fs::read_to_string(join_suffix(path.clone(), "_max"))
            .map_err(Error::MaxSpeedRead)?
            .trim_end()
            .parse()
            .map_err(Error::MaxSpeedParse)?;

        let mut open_options = std::fs::OpenOptions::new();
        open_options.write(true).truncate(true);

        let manual_file = open_options
            .open(join_suffix(path.clone(), "_manual"))
            .map_err(Error::FanOpen)?;

        let output_file = open_options
            .open(join_suffix(path, "_output"))
            .map_err(Error::FanOpen)?;

        let this = Self {
            manual_file,
            output_file,
            config,
            min_speed,
            max_speed,
            sensors,
        };

        println!("Found fan: {this:#?}");
        Ok(this)
    }

    pub fn min_speed(&self) -> u32 {
        self.min_speed
    }

    pub fn max_speed(&self) -> u32 {
        self.max_speed
    }

    pub fn config(&self) -> &FanConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: FanConfig) {
        self.config = config;
    }

    /// Returns the max temp across this fan's sensors, looked up from the
    /// per-tick `SensorReadings` produced by `SensorPool::read_all`.
    pub fn compute_max_temp(&self, readings: &SensorReadings) -> u8 {
        let mut max_temp = 0u8;
        for sensor in &self.sensors {
            let temp = match sensor {
                SensorIdx::Cpu => readings
                    .cpu
                    .expect("CPU sensor configured but CPU temp not read"),
                SensorIdx::Hwmon(i) => readings.hwmons[*i],
                SensorIdx::Nvidia(i) => readings.nvidia[*i],
            };
            max_temp = max_temp.max(temp);
        }
        max_temp
    }

    pub fn set_manual(&self, enabled: bool) -> Result<()> {
        (&self.manual_file)
            .write_all(if enabled { b"1" } else { b"0" })
            .map_err(Error::FanWrite)
    }

    pub fn set_speed(&self, mut speed: u32) -> Result<()> {
        if speed < self.min_speed {
            speed = self.min_speed;
        } else if speed > self.max_speed {
            speed = self.max_speed;
        }

        {
            let mut stdout = std::io::stdout().lock();
            if stdout.is_terminal() {
                print!("\x1b[1K\rSetting fan speed to {speed}");
                let _ = stdout.flush();
            }
        }

        write!(&self.output_file, "{speed}").map_err(Error::FanWrite)?;
        Ok(())
    }

    pub fn calc_speed(&self, temp: u8) -> u32 {
        if self.config.always_full_speed {
            return self.max_speed;
        }

        if temp <= self.config.low_temp {
            return self.min_speed;
        }
        if temp >= self.config.high_temp {
            return self.max_speed;
        }

        let temp = temp as u32;
        let low_temp = self.config.low_temp as u32;
        let high_temp = self.config.high_temp as u32;
        match self.config.speed_curve {
            SpeedCurve::Linear => {
                ((temp - low_temp) as f32 / (high_temp - low_temp) as f32
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
            SpeedCurve::Exponential => {
                let exp = self.config.exp_pow;
                (((temp - low_temp) as f32).powf(exp)
                    / ((high_temp - low_temp) as f32).powf(exp)
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
            SpeedCurve::Logarithmic => {
                (((temp - low_temp) as f32).log((high_temp - low_temp) as f32)
                    * (self.max_speed - self.min_speed) as f32) as u32
                    + self.min_speed
            }
        }
    }
}
