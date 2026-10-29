# Switch Swap

Switch Swap is a Rust and GTK 4 desktop application for CachyOS and other Linux distributions. It detects up to four compatible game controllers and, with one click, switches their face-button layout between Xbox and Nintendo Switch conventions.

## What it does

The privileged `switch-swap.service` reads controller events through Linux evdev. When a layout is enabled, the service temporarily grabs each physical controller so games do not see both the original and remapped input, then creates a corresponding virtual controller through uinput. The GUI offers Xbox layout, Nintendo Switch layout, and Undo buttons. Undo releases the physical controllers and removes their virtual replacements.

The Xbox layout passes standard Linux face-button positions through unchanged. The Nintendo Switch layout swaps south/east and west/north, matching the different A/B and X/Y positions. Sticks, triggers, and other supported keys/axes are passed through. The setting is persisted in `/etc/switch-swap/settings.conf` and restored when the service starts after reboot.

The application supports at most four evdev gamepad interfaces at once. It scans continuously, so compatible controllers connected later are picked up automatically. Device, access, uinput, and event-forwarding errors are exposed in the GUI and service journal.

This initial version does not relay controller rumble/force-feedback or LEDs to the physical device.

## Requirements

- CachyOS/Arch Linux with Rust and Cargo
- GTK 4 development libraries (`gtk4`)
- Linux kernel support for evdev and uinput (`/dev/uinput`)
- `polkit` and a desktop polkit authentication agent for GUI Apply/Undo buttons

Controllers must be exposed by the kernel as evdev gamepads with the standard four face-button codes and X/Y axes. Wired, Bluetooth, and Xbox Wireless Adapter devices can be remapped when the installed kernel and drivers expose them this way. A particular adapter, controller, or game may need additional driver/game-specific configuration; this app cannot remap devices the kernel does not expose.

## Build and install

All commands below are run from this project directory:

```sh
cargo test
cargo build --release
sudo ./install.sh
switch-swap
```

The installer installs the executable and systemd service under `/usr/bin` and `/etc`, then enables the service. It requires root privileges and is intentionally not run by the development build. Apply/Undo prompts for administrator authorization using polkit; do not launch the GUI as root.

Prebuilt x86_64 `.deb`, `.rpm`, and AppImage files are attached to the [GitHub Releases](https://github.com/sweatshopking/switch-swap/releases) page. Debian/Ubuntu users can install the `.deb` with `sudo apt install ./switch-swap_VERSION_amd64.deb`; Fedora-compatible users can use `sudo dnf install ./switch-swap-VERSION-1.x86_64.rpm`. These packages install and enable the system service.

On CachyOS, download and launch the AppImage as your regular user. To enable its persistent, system-wide remapping service, also download `install-appimage.sh` from that release and run `sudo bash install-appimage.sh ./Switch-Swap-VERSION-x86_64.AppImage`. This copies the AppImage into `/opt/switch-swap` so systemd can use it after reboot. To remove the AppImage-based service, run `sudo systemctl disable --now switch-swap.service`, then remove `/etc/systemd/system/switch-swap.service` and `/opt/switch-swap/switch-swap.AppImage`.

The published binaries target x86_64. `SHA256SUMS` is included with each release. Package installation starts the service with remapping disabled by default; choose a layout in the GUI to enable it.

## Behavior

- **Xbox layout**: passes the standard Linux face-button positions through unchanged.
- **Nintendo Switch layout**: swaps south/east and west/north face-button events.
- **Undo remapping**: disables persistence, ungrabs physical controllers, and removes virtual controllers.
- The service scans for newly connected controllers and drops disconnected devices. It reports device access, uinput, grab, and forwarding errors in the GUI/service journal.
- Remapping is system-wide. Any local user can see service status; only root can apply or disable it. The GUI uses `pkexec` for those actions.

On Linux, ordinary user access to controller input can vary with seat/session ACLs and distribution policy. The privileged service needs evdev and uinput access. For diagnostics, check `systemctl status switch-swap.service` and inspect its journal with `journalctl -u switch-swap.service`. The GUI also reports a clear message if the service or `pkexec` is unavailable.
