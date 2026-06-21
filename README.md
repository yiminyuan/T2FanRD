# T2FanRD

Fan Daemon for the Mac Pro 2019 (MacPro7,1), based on the [original version](https://github.com/GnomedDev/T2FanRD).

## Requirements

Linux **kernel 7.1 or newer**, where the T2 SMC is driven by the `macsmc`
stack (`macsmc_hwmon`). The old `applesmc` driver must be blacklisted (it
can't reach a T2 SMC and slows boot); a drop-in is provided in step 2. The
daemon controls fans through `macsmc_hwmon`'s `fanN_target` nodes, which the
kernel exposes **read-only unless** the module is loaded with `fan_control=1`
(see step 2).

## Compilation
`cargo build --release`

## Installation
1. Install the `target/release/t2fanrd` executable to `/usr/bin/t2fanrd` (the path the bundled systemd unit expects):
   ```
   sudo install -m 0755 target/release/t2fanrd /usr/bin/t2fanrd
   ```
2. Blacklist `applesmc`, and enable manual fan control in `macsmc_hwmon` by installing the modprobe drop-ins, then reload the module so `fan_control` takes effect (a reboot also works):
   ```
   sudo install -m 0644 systemd/applesmc-blacklist.conf /etc/modprobe.d/applesmc-blacklist.conf
   sudo install -m 0644 systemd/macsmc_hwmon.conf /etc/modprobe.d/macsmc_hwmon.conf
   sudo modprobe -r macsmc_hwmon; sudo modprobe macsmc_hwmon
   ```
   Confirm it took effect — `fanN_target` should now be writable (`0644`):
   ```
   ls -l /sys/class/hwmon/hwmon*/fan1_target
   ```
   Without this, the daemon exits at startup with a "fan control is disabled" error.
3. Install the systemd unit from `systemd/t2fanrd.service` to `/etc/systemd/system/t2fanrd.service`, then enable and start it:
   ```
   sudo install -m 0644 systemd/t2fanrd.service /etc/systemd/system/t2fanrd.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now t2fanrd.service
   ```
   The unit runs `t2fanrd` as root with `Restart=always` and writes its PID to `/run/t2fand.pid`. On any exit — stop, restart, reboot, shutdown — the daemon hands the fans back to SMC auto; an `ExecStopPost` repeats this as a best-effort net in case the daemon is killed before it can.

## A note on fan handoff back to the SMC

When the daemon exits it releases each controlled fan to SMC auto (writes `0` to `fanN_target`). On this hardware the SMC firmware does **not** fully restore a clean auto curve after a fan has been under manual control — a released fan tends to come back elevated (the CPU fan especially can run high) until either the daemon takes over again or macOS performs a full SMC init. This is an SMC firmware limitation with no Linux-side fix (the driver exposes only manual-on/off and target; there is no reset primitive). If this bothers you, set `auto=true` for the affected fan so the daemon never puts it into manual — that fan then stays under the SMC's own clean auto control.

## Configuration
Initial configuration will be done automatically on first run.

For manual config, the config file can be found at `/etc/t2fand.conf`. A reference config for a Mac Pro 2019 with two MPX modules is included at `systemd/t2fand.conf` — copy it to `/etc/t2fand.conf` to skip the auto-generated defaults.

### Reloading without restart
After editing `/etc/t2fand.conf`, apply the changes in place with:
```
sudo systemctl reload t2fanrd.service
```
This sends `SIGHUP` to the running daemon, which re-reads the config and updates curve parameters (`low_temp`, `high_temp`, `speed_curve`, `exp_pow`, `always_full_speed`) on each tracked fan without releasing them to SMC. Sensor history is preserved across the reload.

**Structural changes require a restart**: if you toggle `auto` for any fan or change the `sensors` list, the daemon will log the rejection and keep its current config — you'll need `sudo systemctl restart t2fanrd.service` for those edits to take effect.

Each fan has the following options.
|        Key        |                            Value                            |
|:-----------------:|:-----------------------------------------------------------:|
|       auto        | If `true`, the fan is left under SMC control and **all other options for that fan are ignored**. Default `false`. |
|      low_temp     |        Temperature that will trigger higher fan speed       |
|     high_temp     |         Temperature that will trigger max fan speed         |
|    speed_curve    |   Three options present. Will be explained in table below.  |
| always_full_speed | if set "true", the fan will be at max speed no matter what. |
|      sensors      | Comma-separated list of `cpu` and/or `slot:<N>` entries. **Mandatory when `auto=false`.** See below. |
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
The `sensors` key declares which temperature sensors drive the fan. The fan responds to the highest temperature across every entry in the list. The supported entry formats are:

- `cpu` — read from `/sys/devices/platform/coretemp.0/hwmon/hwmon*/temp1_input`.
- `slot:<N>` — read every **GPU** hwmon `temp1_input` downstream of physical slot `<N>` (as listed in `/sys/bus/pci/slots/`). Only devices with PCI class `0x03` (display controllers) are matched, so incidental devices in the slot's sub-tree (ethernet, audio, NVMe) are ignored. Multi-die GPUs (e.g. the W6800X Duo) expose one `temp1_input` per die; all of them are included automatically.

The two formats can be combined, e.g. `sensors=cpu,slot:1` for a fan that should respond to whichever is hotter between CPU and slot 1.

`sensors` is **mandatory** when `auto=false`; the daemon refuses to start otherwise. (When `auto=true`, the fan stays on SMC and `sensors` is ignored.)

When two MPX modules are cross-connected via Infinity Fabric Link, the firmware reports each card's dies as reachable from both slots. The daemon disambiguates by attributing each GPU to the slot whose address appears **closest to the PCI root** in the device's canonical path — i.e. its physical slot. You don't need to do anything special; this is automatic.

You can find physical slot numbers with:
```
ls /sys/bus/pci/slots/
```

### Example
Config for a Mac Pro 2019 with dual W6800X Duo MPX modules in slots 1 and 3. Fan 2 is assigned to the MPX module in slot 1, and Fan 3 to the MPX module in slot 3. Each slot contains two GPU dies, and the fan responds to whichever die is hotter.

```ini
# Rear exhaust fan
[Fan1]
auto=true

# Front intake fan - bottom
[Fan2]
auto=false
low_temp=50
high_temp=85
speed_curve=exponential
always_full_speed=false
sensors=slot:1
exp_pow=3.0

# Front intake fan - middle
[Fan3]
auto=false
low_temp=50
high_temp=85
speed_curve=exponential
always_full_speed=false
sensors=slot:3
exp_pow=3.0

# Front intake fan - top
[Fan4]
auto=false
low_temp=55
high_temp=80
speed_curve=exponential
always_full_speed=false
sensors=cpu
```
