use std::{
    io::{IsTerminal, Write},
    os::unix::fs::{FileExt, PermissionsExt},
    path::PathBuf,
};

use crate::{
    config::{FanConfig, SpeedCurve},
    error::{Error, Result},
};

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
}

/// Daemon-level pool of unique sensor file handles. Each unique CPU /
/// hwmon `temp1_input` is opened once and read once per tick, regardless of
/// how many fans reference it.
#[derive(Debug)]
pub struct SensorPool {
    pub cpu_file: Option<std::fs::File>,
    pub hwmons: Vec<std::fs::File>,
}

#[derive(Debug)]
pub struct SensorReadings {
    pub cpu: Option<u8>,
    pub hwmons: Vec<u8>,
}

impl SensorPool {
    pub fn read_all(&self) -> Result<SensorReadings> {
        let cpu = match &self.cpu_file {
            Some(f) => Some(read_temp_file(f)?),
            None => None,
        };
        let mut hwmons = Vec::with_capacity(self.hwmons.len());
        for f in &self.hwmons {
            hwmons.push(read_temp_file(f)?);
        }
        Ok(SensorReadings { cpu, hwmons })
    }
}

#[derive(Debug)]
pub struct FanController {
    target_file: std::fs::File,
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

        let target_path = join_suffix(path, "_target");

        // macsmc creates fanN_target read-only unless the module was loaded
        // with fan_control=1; the mode is fixed at probe time, so a read-only
        // node means this fan cannot be driven. Fail loudly instead of
        // silently writing to a file the kernel will reject.
        let writable = std::fs::metadata(&target_path)
            .map_err(Error::FanOpen)?
            .permissions()
            .mode()
            & 0o200
            != 0;
        if !writable {
            return Err(Error::FanControlDisabled);
        }

        let target_file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&target_path)
            .map_err(Error::FanOpen)?;

        let this = Self {
            target_file,
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
            };
            max_temp = max_temp.max(temp);
        }
        max_temp
    }

    /// Hand this fan back to the SMC's own curve. Writing `0` to `fanN_target`
    /// makes macsmc clear the per-fan manual mode key (`F{i}Md` = 0).
    pub fn release_to_auto(&self) -> Result<()> {
        (&self.target_file).write_all(b"0").map_err(Error::FanWrite)
    }

    /// Drive this fan at `speed` RPM. Writing a value in [min, max] to
    /// `fanN_target` makes macsmc engage manual mode (`F{i}Md` = 1) and set the
    /// target; the daemon never writes 0 here, so it cannot accidentally
    /// release to auto.
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

        write!(&self.target_file, "{speed}").map_err(Error::FanWrite)?;
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
