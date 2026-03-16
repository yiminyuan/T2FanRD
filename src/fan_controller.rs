use std::{
    io::{Read, Seek, Write},
    path::PathBuf,
};

use crate::{
    config::{FanConfig, SpeedCurve},
    error::{Error, Result},
};

pub(crate) fn read_temp_file(temp_file: &mut std::fs::File, temp_buf: &mut String) -> Result<u8> {
    temp_file
        .read_to_string(temp_buf)
        .map_err(Error::TempRead)?;

    temp_file.rewind().map_err(Error::TempSeek)?;

    let temp = temp_buf.trim_end().parse::<u32>().map_err(Error::TempParse);
    temp_buf.clear();
    temp.map(|t| (t / 1000) as u8)
}

#[derive(Debug)]
pub struct FanController {
    manual_file: std::fs::File,
    output_file: std::fs::File,
    config: FanConfig,

    min_speed: u32,
    max_speed: u32,
    sensor_files: Vec<std::fs::File>,
}

impl FanController {
    pub fn new(path: PathBuf, config: FanConfig, sensor_files: Vec<std::fs::File>) -> Result<Self> {
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
            sensor_files,
        };

        println!("Found fan: {this:#?}");
        Ok(this)
    }

    /// Read the maximum temperature across all custom sensors for this fan.
    /// Returns `None` if no custom sensors are configured (use default CPU/GPU temp).
    pub fn read_sensor_temp(&mut self, temp_buf: &mut String) -> Result<Option<u8>> {
        if self.sensor_files.is_empty() {
            return Ok(None);
        }

        let mut max_temp = 0u8;
        for sensor_file in &mut self.sensor_files {
            let temp = read_temp_file(sensor_file, temp_buf)?;
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
                ((temp - low_temp).pow(3) as f32 / (high_temp - low_temp).pow(3) as f32
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
