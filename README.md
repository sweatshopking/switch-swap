# Switch Swap

<p align="center">
  <img src="packaging/switch-swap-logo.svg" alt="Switch Swap" width="420">
</p>

Switch Swap is a Rust and GTK 4 desktop application for CachyOS and other Linux distributions. It detects up to four compatible game controllers and lets you choose which individual controller to remap between Xbox and Nintendo Switch conventions.

## What it does

The privileged `switch-swap.service` reads controller events through Linux evdev. Select a controller from the GUI before choosing Xbox, Nintendo Switch, or Undo. Only that controller is changed; other connected controllers keep their current behavior. When selected, the service temporarily grabs that physical controller so games do not see both the original and remapped input, then creates a corresponding virtual controller through uinput.

The Xbox layout passes standard Linux face-button positions through unchanged. The Nintendo Switch layout swaps south/east and west/north, matching the different A/B and X/Y positions. Sticks, triggers, and other supported keys/axes are passed through. Each controller's layout is persisted separately in `/etc/switch-swap/settings.conf` and restored when that controller reconnects or the service starts after reboot.

The application supports up to four evdev gamepad interfaces at once. It scans continuously, so compatible controllers connected later are picked up automatically. Device, access, uinput, and event-forwarding errors are exposed in the GUI and service journal. Unsupported controller-like event devices are also listed with the missing input codes; if the kernel exposes no gamepad interface, the GUI reports how many event nodes it checked.

This initial version does not relay controller rumble/force-feedback or LEDs to the physical device.

## Requirements

- CachyOS/Arch Linux with Rust and Cargo
- GTK 4 development libraries (`gtk4`)
- Linux kernel support for evdev and uinput (`/dev/uinput`)
- `polkit` and a desktop polkit authentication agent for GUI Apply/Undo buttons

Controllers must be exposed by the kernel as evdev gamepads with the standard four face-button codes and X/Y axes on the same event interface. Wired, Bluetooth, and Xbox Wireless Adapter devices can be remapped when the installed kernel and drivers expose them this way. A controller can appear connected in Bluetooth or USB settings without the kernel creating a gamepad event device; the service reports that distinction and suggests checking `journalctl -k` for driver initialization failures. A particular adapter, controller, or game may need additional driver/game-specific configuration; this app cannot remap devices the kernel does not expose.

## Build and install

All commands below are run from this project directory:

```sh
cargo test
cargo build --release
./target/release/switch-swap
```

On first launch, the setup screen explains the steps. Choose **Install/update system service** to request administrator approval through polkit. Only after approval does the installer copy or update the app at `/usr/bin` (or `/opt/switch-swap` for an AppImage), add its systemd service and default settings, and enable the service. Remapping remains off until you select a controller and choose a layout. Do not launch the GUI as root.

When updating from an older release that applied a layout globally, the updated service disables that legacy global rule for safety. Reinstall/update the service from the GUI; then choose layouts for controllers individually.

Prebuilt x86_64 `.deb`, `.rpm`, and AppImage files are attached to the [GitHub Releases](https://github.com/sweatshopking/switch-swap/releases) page. Debian/Ubuntu users can install the `.deb` with `sudo apt install ./switch-swap_VERSION_amd64.deb`; Fedora-compatible users can use `sudo dnf install ./switch-swap-VERSION-1.x86_64.rpm`. These packages install and enable the system service.

On CachyOS, download and launch the AppImage as your regular user, then choose **Install system service** in the setup screen and approve the polkit prompt. This copies the AppImage into `/opt/switch-swap` so systemd can use it after reboot. The release also includes `install-appimage.sh` for manual installation. To remove the AppImage-based service, run `sudo systemctl disable --now switch-swap.service`, then remove `/etc/systemd/system/switch-swap.service` and `/opt/switch-swap/switch-swap.AppImage`.

The published binaries target x86_64. `SHA256SUMS` is included with each release. Package installation starts the service with remapping disabled by default; choose a layout in the GUI to enable it.

## Behavior

- **Xbox layout**: passes the standard Linux face-button positions through unchanged.
- **Nintendo Switch layout**: swaps south/east and west/north face-button events.
- **Undo selected controller**: removes only that controller's saved profile, ungrabs it, and removes its virtual replacement.
- The service scans for newly connected controllers and drops disconnected devices. It reports device access, uinput, grab, and forwarding errors in the GUI/service journal.
- The service is system-wide, but each remapping profile is tied to one controller identity. Any local user can see status; only root can apply or undo a profile. The GUI uses `pkexec` for those actions.

On Linux, ordinary user access to controller input can vary with seat/session ACLs and distribution policy. The privileged service needs evdev and uinput access. For diagnostics, check `systemctl status switch-swap.service` and inspect its journal with `journalctl -u switch-swap.service`. The GUI also reports a clear message if the service or `pkexec` is unavailable.
