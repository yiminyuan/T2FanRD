use std::{
    io::{Read, Seek, Write},
    path::PathBuf,
};

use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};

use crate::{
    config::{FanConfig, SpeedCurve},
    error::{Error, Result},
};

/// A temperature sensor source. Either a hwmon sysfs file (AMD, CPU, etc.)
/// or an NVIDIA GPU queried via NVML.
#[derive(Debug)]
pub enum TempSensor {
    /// A hwmon `temp*_input` file that reports millidegrees Celsius.
    Hwmon(std::fs::File),
    /// An NVIDIA GPU identified by its PCI bus ID (e.g. `0000:01:00.0`).
    /// Temperature is read via NVML.
    Nvml(String),
}

pub(crate) fn read_temp_file(temp_file: &mut std::fs::File, temp_buf: &mut String) -> Result<u8> {
    temp_file
        .read_to_string(temp_buf)
        .map_err(Error::TempRead)?;

    temp_file.rewind().map_err(Error::TempSeek)?;

    let temp = temp_buf.trim_end().parse::<u32>().map_err(Error::TempParse);
    temp_buf.clear();
    temp.map(|t| (t / 1000) as u8)
}

/// Read the temperature of an NVIDIA GPU via NVML.
/// The `bus_id` should be in normalized PCI format (e.g. `0000:01:00.0`).
fn read_nvml_temp(nvml: &Nvml, bus_id: &str) -> Result<u8> {
    // device_by_pci_bus_id requires S where Vec<u8>: From<S>, which is
    // satisfied by String but not &str, so we need an owned copy.
    let device = nvml.device_by_pci_bus_id(bus_id.to_owned())?;
    let temp = device.temperature(TemperatureSensor::Gpu)?;
    Ok(temp as u8)
}

#[derive(Debug)]
pub struct FanController {
    manual_file: std::fs::File,
    output_file: std::fs::File,
    config: FanConfig,

    min_speed: u32,
    max_speed: u32,
    sensors: Vec<TempSensor>,
}

impl FanController {
    pub fn new(path: PathBuf, config: FanConfig, sensors: Vec<TempSensor>) -> Result<Self> {
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

    /// Read the maximum temperature across all custom sensors for this fan.
    /// Returns `None` if no custom sensors are configured (use default CPU temp).
    pub fn read_sensor_temp(
        &mut self,
        temp_buf: &mut String,
        nvml: Option<&Nvml>,
    ) -> Result<Option<u8>> {
        if self.sensors.is_empty() {
            return Ok(None);
        }

        let mut max_temp = 0u8;
        for sensor in &mut self.sensors {
            let temp = match sensor {
                TempSensor::Hwmon(file) => read_temp_file(file, temp_buf)?,
                TempSensor::Nvml(bus_id) => {
                    let nvml = nvml.expect("NVML sensor present but NVML not initialized");
                    read_nvml_temp(nvml, bus_id)?
                }
            };
            max_temp = max_temp.max(temp);
        }
        Ok(Some(max_temp))
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

        print!("\x1b[1K\rSetting fan speed to {speed}");
        let _ = std::io::stdout().lock().flush();

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
