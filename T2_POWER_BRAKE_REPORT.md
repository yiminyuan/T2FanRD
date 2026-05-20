# Mac Pro 2019: T2 SMC asserts `HW_POWER_BRAKE` on NVIDIA GPU when ≥2 fans are in manual mode

Forum thread with frametime graph and benchmark screenshots: https://discuss.cachyos.org/t/games-start-dropping-frames-in-a-certain-pattern-after-a-few-minutes-of-gameplay/28428

## Summary

On a Mac Pro 2019 (`MacPro7,1`) with the T2 SMC kernel patches, putting two or more fans of the T2 fan controller (`APP0001:00`) into manual mode causes the SMC to begin asserting `HW_POWER_BRAKE` on the NVIDIA add-in GPU. The brake oscillates at the SMC's internal cadence, capping graphics power at roughly 50% of TDP and producing a sharp periodic frametime spike pattern. One fan in manual is fine; two or more triggers it. CUDA workloads are not visibly impacted.

The bug is reproducible with no userspace daemon — bare `echo 1 > fan*_manual` writes are sufficient. It started after a kernel/driver bundle update on 2026-04-23.

## Environment

| | |
|---|---|
| Hardware | Mac Pro 2019 (`MacPro7,1`) |
| GPU | NVIDIA RTX PRO 6000 Blackwell Workstation Edition (600 W TDP) |
| Distribution | CachyOS Linux |
| Kernel | `linux-cachyos 7.0.1-1` (also reproduces on `linux-cachyos-lts 6.18.24-1`) |
| Driver | `linux-cachyos-nvidia-open 7.0.1-1` (NVIDIA Open Kernel Module 595.58.03) |

## Reproducer

```sh
# In one terminal: put two fans in manual mode at min PWM
for i in 2 3; do
    BASE=$(echo /sys/devices/pci*/*/*/*/APP0001:00/fan${i}_input | sed 's/_input//')
    MIN=$(cat ${BASE}_min)
    echo $MIN | sudo tee ${BASE}_output
    echo 1   | sudo tee ${BASE}_manual
done

# In another terminal: run any GPU-bound graphics workload (Wukong, Heaven, etc.)

# In a third: watch throttle reasons
nvidia-smi --query-gpu=clocks_throttle_reasons.active,clocks_throttle_reasons.hw_power_brake_slowdown,power.draw,power.limit,temperature.gpu --format=csv -l 1
```

Restore: `echo 0 | sudo tee ${BASE}_manual` for each fan.

## Observed behavior

`nvidia-smi` shows `HW_POWER_BRAKE` flipping `Active` / `Not Active` continuously while ≥2 fans are manual. Power tracks the brake — ~210 W when active, ~330 W when not, against a 600 W limit. GPU temperature stays at 58–65 °C, well below thermal slowdown.

Sample output:
```
0x0000000000000088, Active,     201.40 W, 600.00 W, 57
0x0000000000000000, Not Active, 354.03 W, 600.00 W, 61
0x0000000000000088, Active,     275.92 W, 600.00 W, 58
0x000000000000008C, Active,     391.69 W, 600.00 W, 60
0x0000000000000000, Not Active, 367.19 W, 600.00 W, 65
```

Bitfield decoding: `0x80` = `HwPowerBrakeSlowdown`, `0x08` = `HwSlowdown` (aggregate), `0x04` = `SwPowerCap`. So `0x88` = power brake, `0x8C` = power brake + sw cap, `0x00` = no throttle.

In Black Myth: Wukong (4K, DLSS Quality, all Cinematic): average 94 fps with the bug active vs ~100+ fps without. 95th-percentile-low frame rate drops to 44 fps with dense periodic dips matching the brake oscillation.

## Threshold

Single-fan tests on fan1 and fan2 individually: smooth. Two-fan tests (any pair of `2 3 4`): triggers. Three or four fans: triggers. The boundary is exact at 2.

## Diagnostics ruled out

| Hypothesis | Test | Result |
|---|---|---|
| Userspace daemon | Bare `echo` to sysfs, no daemon | Reproduces |
| Daemon runtime activity | `kill -STOP <pid>` mid-game | No change |
| NVML polling / init | Disabled `Nvml::init()` entirely | No change |
| Stuck SMC firmware state | Power-off + unplug 15+ s + replug | No change |
| 7.0.x-only regression | Tested LTS 6.18.24 | Reproduces on LTS too |

## Suspected mechanism

The T2 SMC firmware seems to treat ≥2 fans in manual mode as evidence the OS has taken over thermal management, and asserts the GPU's PCIe `POWER_BRAKE#` pin defensively. CUDA escapes because steady-state compute already runs below the brake-asserted power level. The behavior likely pre-existed in firmware indefinitely; what changed in the recent update is something on the kernel/driver side that now propagates the brake signal effectively (possibly the applesmc-t2 driver's interaction with SMC fan-control state, or the NVIDIA Open Kernel Module's response to brake assertions).

The suspect transaction (2026-04-23 22:59 PT, installed simultaneously):
- `linux-cachyos 7.0.0-1 → 7.0.1-1`
- `linux-cachyos-nvidia-open 7.0.0-1 → 7.0.1-1`
- `linux-cachyos-lts 6.18.22-1 → 6.18.24-1`
- `linux-cachyos-lts-nvidia-open 6.18.22-1 → 6.18.24-1`

A rollback to the 7.0.0 bundle (CachyOS PKGBUILD commit `c64cf231`) is in progress to confirm this is a regression rather than longstanding behavior.

## Workarounds

1. Stop the t2 fan daemon (`sudo systemctl stop t2fanrd`).
2. Restore T2 fan controller to SMC auto mode (`sudo systemctl enable t2-fans-smc-auto`).

## Notes

The userspace daemon I'm running is [t2fanrd](https://github.com/yiminyuan/T2FanRD) (forked from `GnomedDev/T2FanRD`). The original upstream `t2linux/t2fand` daemon also flips all fans to manual at startup and should exhibit identical behavior.
