# wluma

A tool for Wayland compositors to automatically adjust screen brightness based on the screen contents and amount of ambient light around you.

## Supported screen capture protocols

With the default `capturer = "auto"`, `wluma` first looks for a supported Wayland capture protocol, then tries PipeWire, and falls back to using ambient light only if neither is available. See the "Configuration" section below for details.

The list of supported protocols:

- `ext-image-copy-capture-v1` - the newest protocol that potentially is (or will be) supported by any modern Wayland desktop environment.
  - requires `ext-image-capture-source-v1` and `linux-dmabuf-v1` protocols to be supported as well.
- `wlr-screencopy-unstable-v1` - supported by any `wlroots`-based compositors (e.g. `sway`), as well as Hyprland.
  - requires `linux-dmabuf-v1` protocol to be supported as well.
- `wlr-export-dmabuf-unstable-v1` - supported by any `wlroots`-based compositors (e.g. `sway`).
- PipeWire streams provided by:
  - KWin's private screencast protocol.
  - Mutter's private ScreenCast API.
  - Generic ScreenCast portal.

## Idea

The app will automatically brighten the screen when you are looking at a dark window (such as a fullscreen terminal) and darken the screen when you are looking at a bright window (such as web browser). The algorithm takes into consideration the amount of ambient light around you, so the same window can be brighter during the day than during the night.

With permission of [Lumen](https://github.com/anishathalye/lumen)'s author (the project that inspired me to create this app), I'm reusing a demo GIF:

![demo](https://user-images.githubusercontent.com/1177900/82347130-8bd22b80-99f7-11ea-8545-0d311240a30d.gif)

## Usage

Simply launch `wluma` and continue adjusting your screen brightness as you usually do - the app will learn your preferences.

`wluma` will not do anything on the first launch! You have to adjust the brightness by hand a few times, in different environment and/or with different screen contents, that way `wluma` will learn your preferences and only then it will begin to automatically change your screen brightness for you.

## Performance

The app has minimal impact on system resources and battery life even though it is able to monitor screen contents several times a second. This is achieved by importing DMA-BUF screen buffers and doing computations on GPU using Vulkan API.

## Installation

<a href="https://repology.org/project/wluma/versions">
  <img src="https://repology.org/badge/vertical-allrepos/wluma.svg" alt="Packaging status" align="right">
</a>

Use one of the available packages and methods below:

- Alpine Linux: [wluma](https://pkgs.alpinelinux.org/packages?name=wluma)
- Arch Linux: [wluma](https://aur.archlinux.org/packages/wluma/) or [wluma-git](https://aur.archlinux.org/packages/wluma-git/)
- NixOS: [wluma](https://search.nixos.org/packages?channel=unstable&show=wluma&from=0&size=50&sort=relevance&type=packages&query=wluma)
- Fedora Linux: [wluma](https://github.com/terrapkg/packages/tree/frawhide/anda/system/wluma) via the [Terra repository](https://terra.fyralabs.com/)
- Build the app yourself using the instructions below and copy the resulting binary somewhere in your `$PATH`.
  - optionally, grab the `wluma.service` if you want to run it as a systemd-service - it can be placed e.g. in `~/.config/systemd/user/`.
  - you might need `90-wluma-backlight.rules` too, if you want to give `wluma` direct driver access for the fastest performance (see "Permissions" section below) - it can be placed e.g. in `/etc/udev/rules.d/`.

## Build

[![CI](https://github.com/maximbaz/wluma/actions/workflows/ci.yml/badge.svg)](https://github.com/maximbaz/wluma/actions/workflows/ci.yml)

If you want to build the app yourself, make sure you use latest stable Rust, otherwise you might get compilation errors! Using `rustup` is perhaps the easiest. Ubuntu needs the following dependencies: `sudo apt-get -y install v4l-utils libv4l-dev libudev-dev libvulkan-dev libdbus-1-dev libpipewire-0.3-dev`.

Then simply run `cargo build --locked --release` and the binary will be placed into `./target/release/wluma`.

## Permissions

In order to access backlight devices, `wluma` must either:

- have direct driver access: install the supplied `90-wluma-backlight.rules` udev rule, add your user to the `video` group and reboot (fastest, most likely a requirement for smooth transitions)
- run on a system that uses `elogind` or `systemd-logind` (they provide a safe interface for unprivileged users to control device's brightness through `dbus`, no configuration necessary)
- run as `root` (not recommended)

## Configuration

The `config.toml` in repository represents default config values. To change them, copy the file into `$XDG_CONFIG_HOME/wluma/config.toml` and adjust as desired.

### ALS

Choose whether to use a real IIO-based ambient light sensor (`[als.iio]`), a webcam-based simulation (`[als.webcam]`), a time-based simulation (`[als.time]`) or disable it altogether (`[als.none]`).

When `[als.iio]` is configured, wluma uses `iio-sensor-proxy` over the system D-Bus and reports light levels in lux. This is the recommended setup, as it avoids repeatedly polling the sensor through sysfs. Make sure the `iio-sensor-proxy` service is installed and available.

The legacy `path` field is deprecated, it enables direct IIO sysfs polling when `iio-sensor-proxy` is unavailable at startup; remove it once `iio-sensor-proxy` works on your system.

```toml
[als.iio]
path = "/sys/bus/iio/devices" # deprecated direct IIO sysfs polling
thresholds = { 0 = "night", 20 = "dark", 80 = "dim", 250 = "normal", 500 = "bright", 800 = "outdoors" }
```

Each of them contains a `thresholds` field, which comes with good default values. It is there to convert generally exponential lux values into a linear scale to improve the prediction algorithm in `wluma`. Keys are the raw values from ambient light sensor (maximal value depends on the implementation), values are arbitrary "profiles". `wluma` will predict the best screen brightness according to the data learned within the same ALS profile.

### Displays

Multiple outputs are supported, using `backlight` (common for internal laptop screens) and `ddcutil` (for external screens). DDC is known to often be problematic, always consider trying out [ddcci-driver-linux](https://gitlab.com/ddcci-driver-linux/ddcci-driver-linux) first if you can.

The `name` field in the output config identifies the Wayland output, and is matched as a substring against descriptions containing model, manufacturer and serial number (like `eDP-1 'Sharp Corporation 0x14A8 0x00000000' (eDP-1)`), so you are free to put simply `eDP-1`, `HDMI-A-3`, or a unique model/serial substring. It is your responsibility to make sure that the values you use match **uniquely** to one output only.

For `output.ddcutil`, if the identifier that works for brightness control is not the same substring that your Wayland compositor exposes for screen capture, set `identifier`. This is common for external DDC monitors: `name` should match the compositor output description such as `HDMI-A-3` or `LG ULTRAWIDE`, while `identifier` may need to be a serial number for DDC.

_Tip:_ run `wluma` with `RUST_LOG=debug` to see how your outputs are being identified, so that you can choose an appropriate `name` and `identifier` configuration values.

The `capturer` field will determine how screen contents will be captured. Currently supported values are `auto`, `wayland`, `pipewire` and `none` (ignores screen contents and predicts brightness only based on ALS). The default value `auto` tries supported Wayland protocols first, then PipeWire sources, then falls back to `none`. The value `wayland` automatically chooses the most appropriate Wayland protocol, but you can force a specific one with `ext-image-copy-capture-v1`, `wlr-screencopy-unstable-v1` or `wlr-export-dmabuf-unstable-v1`. The value `pipewire` similarly automatically chooses the most appropriate protocol, and you can force a specific source with `zkde-screencast-unstable-v1`, `gnome-mutter-screencast` or `xdg-desktop-portal-screencast`.

_Tip:_ run `wluma` with `RUST_LOG=debug` and `capturer="auto"` to see which protocols are supported and which capturer `wluma` chooses.

When the ScreenCast portal is used, select the monitor matching the configured output on the first run. wluma asks the portal to persist this selection and stores its restore token in the XDG state directory, so supported portal backends can restore it without prompting after restart. A separate portal session and restore token are used for each configured output.

#### Algorithm

The default algorithm that `wluma` uses is called `adaptive`, which is when it learns from you as you continue adjusting brightness manually. It will eventually figure out patterns in how you tend to adjust brightness in dark and lit conditions and depending on what is currently being displayed on the screen, and will beging to do it automatically for you.

If you instead want to preserve control over absolute brightness value, but let `wluma` only do relative adjustments, there is an alternative algorithm called `manual`. It can be useful if you feel like `wluma` is unable to learn the patterns, for example because you don't have a real ambient light sensor, and neither of the alternative ALS inputs are able to capture the real light conditions precisely enough.

Here's how you enable the manual algorithm in the config:

```toml
[als.time]
thresholds = { 0 = "night", 8 = "day", 18 = "night" }

[[output.backlight]]
name = "eDP-1"
path = "/sys/class/backlight/intel_backlight"
capturer = "auto"
[output.backlight.predictor.manual]
thresholds.day = { 0 = 0, 100 = 10 }
thresholds.night = { 0 = 0, 100 = 60 }
```

In other words, you activate the predictor for a given `output` using `[output.backlight.predictor.manual]`, and then you define thresholds for each ALS condition using the following syntax:

```
thresholds.<als threshold name> = {<luma> = <brightness reduction percentage>}
```

- `luma` is the "whiteness" of your screen contents, measured in percentage, from `0` to `100`.
- Current screen brightness (that you set manually) will be reduced by the corresponding `brightness reduction percentage` based on what is currently being displayed on the screen.
- `als threshold name` is the custom name that you define in ALS thresholds. When using `[als.none]`, the `als threshold name` is `none`.
- You can define as many entries within each threshold as you want (up to 100, for every single `luma` value). The algorithm will interpolate between the values you define.

The example config above expresses the following intention:

- During the day, the screen brightness will be reduced upmost by 10% of the value you set - fully black screen does not reduce the brightness at all, fully white screen reduces it by 10%, screen contents with "whiteness" of 70% will reduce the brightness by 7%, etc.
- During the day, the screen brightness will be reduced upmost by 60% of the value you set - using the same logic as above.

## Run

To run the app, simply launch `wluma` or use the provided systemd user service.

## Debugging

To enable logging, set environment variable `RUST_LOG` to one of these values: `error`, `warn`, `info`, `debug`, `trace`.

For more complex selectors, see [env_logger's documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging).

## Validating that wluma is able to see screen contents correctly

This is a useful test to validate that wluma does indeed see the screen contents correctly. This is obviously only applicable if you didn't disable `capturer` in your config.

1. Stop any running `wluma` instances
1. Run the latest code from `main` branch (unless another branch was given to you by the maintainers): `RUST_LOG=trace cargo run`
1. Open https://deadpixeltest.org/ and start the test.
1. Make sure that **the entire screen** is covered with a single solid color, nothing else should be visible - not a status bar nor a notification, nothing else.
1. Repeat for each of these colors: `black`, `white`, `red`, `green`, `blue`:
   1. Let the color be visible for a few seconds.
   1. Quickly go back to the running `wluma` and check the `luma` value reported for that color: `Prediction: 252 (lux: none, luma: ---> 14 <---)`
1. Compare your values with the following expected results:
   ```
   black: 0
   white: 100
   red: 49
   green: 83
   blue: 26
   ```

If your results do not match, please open an issue and let's investigate!

## Known issues (help wanted!)

Help is wanted and much appreciated! If you want to implement some of these, feel free to open an issue and I'll provide more details and try to help you along the way.

- Plugging in a screen while `wluma` is running. Workaround: restart `wluma`.

## Relevant projects

- [lumen](https://github.com/anishathalye/lumen): project that inspired me to create this app
