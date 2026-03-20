# T2FanRD

Fan Daemon for the Mac Pro 2019 (MacPro7,1), based on the [original version](https://github.com/GnomedDev/T2FanRD).

## Compilation
`cargo build --release`

## Installation
1. Copy the `target/release/t2fanrd` executable to wherever your distro wants executables to be run by root.
2. Setup the executable to be run automatically at startup, like via a [systemd service](https://github.com/t2linux/fedora/blob/2947fdc909a35f04eb936a4f9c0f33fe4e52d9c2/t2fanrd/t2fanrd.service).

## Configuration
Initial configuration will be done automatically on first run.

For manual config, the config file can be found at `/etc/t2fand.conf`.

There's six options for each fan.
|        Key        |                            Value                            |
|:-----------------:|:-----------------------------------------------------------:|
|      low_temp     |        Temperature that will trigger higher fan speed       |
|     high_temp     |         Temperature that will trigger max fan speed         |
|    speed_curve    |   Three options present. Will be explained in table below.  |
| always_full_speed | if set "true", the fan will be at max speed no matter what. |
|      sensors      | Comma-separated list of `slot:<N>` specifiers. See below.   |
|      exp_pow      | Exponent for the exponential curve (default: 3). See below. |

For `speed_curve`, there's three options.
|     Key     |                   Value                   |
|:-----------:|:-----------------------------------------:|
|    linear   |     Fan speed will be scaled linearly.    |
| exponential |  Fan speed will be scaled exponentially.  |
| logarithmic | Fan speed will be scaled logarithmically. |

The `exp_pow` option controls the exponent used when `speed_curve` is set to `exponential`. A higher value makes the curve ramp up more aggressively at higher temperatures. The default is 3. `exp_pow=0` would make every temperature map to full speed, and `exp_pow=1` is equivalent to the linear curve. This option has no effect on `linear` or `logarithmic` curves.

Here's an image to better explain the speed curves. (Red: linear, blue: exponential, green: logarithmic)
![Image of fan curve graphs](https://user-images.githubusercontent.com/39993457/233580720-cfdaba12-a2d8-430c-87a2-15209dcfec6d.png)

### Sensors
By default, fan speed is based on CPU temperature. The `sensors` key lets you assign PCIe slot-based temperature sensors to individual fans. The fan will respond to the highest temperature across all sensors matched under the specified slots.

Use the `slot:<N>` format, where `<N>` is a physical PCIe slot number as exposed in `/sys/bus/pci/slots/`. This is stable across PCIe topology changes — adding or removing a PCIe card won't shift PCI bus addresses and break your config.

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
low_temp=55
high_temp=80
speed_curve=exponential
always_full_speed=false
sensors=slot:1
exp_pow=2.5

# Front intake fan - middle
[Fan3]
low_temp=55
high_temp=80
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
