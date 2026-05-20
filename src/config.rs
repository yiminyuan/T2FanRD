use std::{io::ErrorKind, num::NonZeroUsize, str::FromStr};

use crate::{Error, Result};

#[cfg(debug_assertions)]
const CONFIG_FILE: &str = "./t2fand.conf";
#[cfg(not(debug_assertions))]
const CONFIG_FILE: &str = "/etc/t2fand.conf";

#[derive(Clone, Copy, Debug)]
pub enum SpeedCurve {
    Linear,
    Exponential,
    Logarithmic,
}

impl std::fmt::Display for SpeedCurve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Linear => f.write_str("linear"),
            Self::Exponential => f.write_str("exponential"),
            Self::Logarithmic => f.write_str("logarithmic"),
        }
    }
}

impl FromStr for SpeedCurve {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "linear" => Self::Linear,
            "exponential" => Self::Exponential,
            "logarithmic" => Self::Logarithmic,
            _ => return Err(()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SensorSpec {
    Cpu,
    Slot(String),
}

impl std::fmt::Display for SensorSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => f.write_str("cpu"),
            Self::Slot(n) => write!(f, "slot:{n}"),
        }
    }
}

impl FromStr for SensorSpec {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        if s == "cpu" {
            Ok(Self::Cpu)
        } else if let Some(slot) = s.strip_prefix("slot:") {
            if slot.is_empty() || !slot.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::InvalidConfigValue(
                    "sensors (slot number must be digits)",
                ));
            }
            Ok(Self::Slot(slot.to_owned()))
        } else {
            Err(Error::InvalidConfigValue(
                "sensors (expected cpu or slot:<N>)",
            ))
        }
    }
}

#[derive(Clone, Debug)]
pub struct FanConfig {
    pub auto: bool,
    pub low_temp: u8,
    pub high_temp: u8,
    pub speed_curve: SpeedCurve,
    pub always_full_speed: bool,
    pub sensors: Vec<SensorSpec>,
    /// Exponent for the exponential speed curve. Only used when
    /// `speed_curve` is `Exponential`. Defaults to 3.0.
    pub exp_pow: f32,
}

impl FanConfig {
    fn validate(&self) -> Result<()> {
        if self.auto {
            return Ok(());
        }
        if self.sensors.is_empty() {
            return Err(Error::InvalidConfigValue(
                "sensors (required when auto=false)",
            ));
        }
        if self.low_temp >= self.high_temp {
            return Err(Error::InvalidConfigValue(
                "low_temp (must be less than high_temp)",
            ));
        }
        if !self.exp_pow.is_finite() || self.exp_pow <= 0.0 {
            return Err(Error::InvalidConfigValue(
                "exp_pow (must be a positive finite number)",
            ));
        }
        Ok(())
    }

    fn write_property<'a>(
        &self,
        setter: &'a mut ini::SectionSetter<'a>,
    ) -> &'a mut ini::SectionSetter<'a> {
        let mut s = setter.set("auto", self.auto.to_string());
        if self.auto {
            return s;
        }
        let joined_sensors = self
            .sensors
            .iter()
            .map(SensorSpec::to_string)
            .collect::<Vec<_>>()
            .join(",");
        s = s
            .set("low_temp", self.low_temp.to_string())
            .set("high_temp", self.high_temp.to_string())
            .set("speed_curve", self.speed_curve.to_string())
            .set("always_full_speed", self.always_full_speed.to_string())
            .set("sensors", joined_sensors);
        #[allow(clippy::float_cmp)]
        if matches!(self.speed_curve, SpeedCurve::Exponential) && self.exp_pow != 3.0 {
            s = s.set("exp_pow", self.exp_pow.to_string());
        }
        s
    }
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            auto: false,
            low_temp: 55,
            high_temp: 75,
            speed_curve: SpeedCurve::Linear,
            always_full_speed: false,
            sensors: vec![SensorSpec::Cpu],
            exp_pow: 3.0,
        }
    }
}

impl TryFrom<&ini::Properties> for FanConfig {
    type Error = Error;

    fn try_from(properties: &ini::Properties) -> Result<Self, Self::Error> {
        fn get_value<V: FromStr>(properties: &ini::Properties, key: &'static str) -> Result<V> {
            let value_str = properties.get(key).ok_or(Error::MissingConfigValue(key))?;
            value_str
                .parse()
                .map_err(|_| Error::InvalidConfigValue(key))
        }

        let auto = properties
            .get("auto")
            .map(|s| {
                s.parse::<bool>()
                    .map_err(|_| Error::InvalidConfigValue("auto"))
            })
            .transpose()?
            .unwrap_or(false);

        if auto {
            return Ok(Self {
                auto: true,
                ..Self::default()
            });
        }

        let sensors: Vec<SensorSpec> = match properties.get("sensors") {
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::parse)
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };

        let exp_pow = properties
            .get("exp_pow")
            .map(|s| s.parse().map_err(|_| Error::InvalidConfigValue("exp_pow")))
            .transpose()?
            .unwrap_or(3.0);

        let config = Self {
            auto: false,
            low_temp: get_value(properties, "low_temp")?,
            high_temp: get_value(properties, "high_temp")?,
            speed_curve: get_value(properties, "speed_curve")?,
            always_full_speed: get_value(properties, "always_full_speed")?,
            sensors,
            exp_pow,
        };
        config.validate()?;
        Ok(config)
    }
}

fn parse_config_file(file_raw: &str, fan_count: NonZeroUsize) -> Result<Vec<FanConfig>> {
    let file = ini::Ini::load_from_str(file_raw)?;
    let mut configs = Vec::with_capacity(fan_count.get());

    for i in 1..=fan_count.get() {
        let section = file
            .section(Some(format!("Fan{i}")))
            .ok_or(Error::MissingFanConfig(i))?;

        configs.push(FanConfig::try_from(section)?);
    }

    Ok(configs)
}

fn generate_config_file(fan_count: NonZeroUsize) -> Result<Vec<FanConfig>> {
    let mut config_file = ini::Ini::new();
    let mut configs = Vec::with_capacity(fan_count.get());
    for i in 1..=fan_count.get() {
        let config = FanConfig::default();

        let mut setter = config_file.with_section(Some(format!("Fan{i}")));
        config.write_property(&mut setter);

        configs.push(config);
    }

    config_file
        .write_to_file(CONFIG_FILE)
        .map_err(Error::ConfigCreate)?;

    Ok(configs)
}

pub fn load_fan_configs(fan_count: NonZeroUsize) -> Result<Vec<FanConfig>> {
    match std::fs::read_to_string(CONFIG_FILE) {
        Ok(file_raw) => parse_config_file(&file_raw, fan_count),
        Err(err) if err.kind() == ErrorKind::NotFound => generate_config_file(fan_count),
        Err(err) => Err(Error::ConfigRead(err)),
    }
}
