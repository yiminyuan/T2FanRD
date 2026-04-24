# T2FanRD

Fan Daemon for the Mac Pro 2019 (MacPro7,1), based on the [original version](https://github.com/GnomedDev/T2FanRD).

## Compilation
`cargo build --release`

## Installation
1. Install the `target/release/t2fanrd` executable to `/usr/bin/t2fanrd` (the path the bundled systemd unit expects):
   ```
   sudo install -m 0755 target/release/t2fanrd /usr/bin/t2fanrd
   ```
2. Install the systemd unit from `systemd/t2fanrd.service` to `/etc/systemd/system/t2fanrd.service`, then enable and start it:
   ```
   sudo install -m 0644 systemd/t2fanrd.service /etc/systemd/system/t2fanrd.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now t2fanrd.service
   ```
   The unit runs `t2fanrd` as root with `Restart=always` and writes its PID to `/run/t2fand.pid`.

## Configuration
Initial configuration will be done automatically on first run.

For manual config, the config file can be found at `/etc/t2fand.conf`. A reference config for a Mac Pro 2019 with two MPX modules is included at `systemd/t2fand.conf` — copy it to `/etc/t2fand.conf` to skip the auto-generated defaults.

There's six options for each fan.
|        Key        |                            Value                            |
|:-----------------:|:-----------------------------------------------------------:|
|      low_temp     |        Temperature that will trigger higher fan speed       |
|     high_temp     |         Temperature that will trigger max fan speed         |
|    speed_curve    |   Three options present. Will be explained in table below.  |
| always_full_speed | if set "true", the fan will be at max speed no matter what. |
|      sensors      | Comma-separated list of `slot:<N>` specifiers. See below.   |
|      exp_pow      | Exponent for the exponential curve (default: 3, accepts decimals). See below. |

For `speed_curve`, there's three options.
|     Key     |                   Value                   |
|:-----------:|:-----------------------------------------:|
|    linear   |     Fan speed will be scaled linearly.    |
| exponential |  Fan speed will be scaled exponentially.  |
| logarithmic | Fan speed will be scaled logarithmically. |

The `exp_pow` option controls the exponent used when `speed_curve` is set to `exponential`. It accepts decimal values (e.g. `exp_pow=2.5`). A higher value makes the curve ramp up more aggressively at higher temperatures. The default is 3. `exp_pow=1` is equivalent to the linear curve. This option has no effect on `linear` or `logarithmic` curves.

Here's an image to better explain the speed curves. (Red: linear, blue: exponential, green: logarithmic)
![Image of fan curve graphs](https://user-images.githubusercontent.com/39993457/233580720-cfdaba12-a2d8-430c-87a2-15209dcfec6d.png)

### Sensors
By default, fan speed is based on CPU temperature. The `sensors` key lets you assign PCIe slot-based temperature sensors to individual fans. The fan will respond to the highest temperature across all sensors matched under the specified slots.

Use the `slot:<N>` format, where `<N>` is a physical PCIe slot number as exposed in `/sys/bus/pci/slots/`. This is stable across PCIe topology changes — adding or removing a PCIe card won't shift PCI bus addresses and break your config.

Both AMD (hwmon) and NVIDIA GPUs are supported transparently. For AMD GPUs, temperatures are read from `/sys/class/hwmon/`. For NVIDIA GPUs, temperatures are queried via the NVIDIA Management Library (NVML). If the NVIDIA driver is not installed, NVIDIA detection is silently skipped.

You can find physical slot numbers with:
```
ls /sys/bus/pci/slots/
```

### Example
Config for a Mac Pro 2019 with dual W6800X Duo MPX modules in slots 1 and 3. Fan 2 is assigned to the MPX module in slot 1, and Fan 3 to the MPX module in slot 3. Each slot contains two GPU dies, and the fan responds to whichever die is hotter.

```ini
# Rear exhaust fan
[Fan1]
low_temp=55
high_temp=80
speed_curve=exponential
always_full_speed=false

# Front intake fan - bottom
[Fan2]
low_temp=50
high_temp=85
speed_curve=exponential
always_full_speed=false
sensors=slot:1
exp_pow=2.5

# Front intake fan - middle
[Fan3]
low_temp=50
high_temp=85
speed_curve=exponential
always_full_speed=false
sensors=slot:3
exp_pow=2.5

# Front intake fan - top
[Fan4]
low_temp=55
high_temp=80
speed_curve=exponential
always_full_speed=false
```
