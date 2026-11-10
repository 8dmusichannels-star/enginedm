# Changelog

## enginedm-rs

A forked Hyprdm EngineDM project. The C/C++ and Quickshell/QML parts were rewritten in Rust and HTML/CSS.

### Architecture

- `crates/edm-greeter`: greeter written in Rust.
- `interfaces/Hyproman/{main.html,style.css}`: interface rendered by **Blitz**.
- `crates/edm-core`: core logic (config, accounts, OTP, session supervision, greetd client, PAM).
- `crates/edm-configmanager`: configuration manager.
- Builds use plain `cargo build` / `install.sh`.

### Login and authentication

- Login authenticates the user named in the username field and starts the session.
- 2FA (TOTP and HOTP) is supported: codes are compared with zero padding, a ±1 step window covers clock drift, and HOTP uses look-ahead.
- A misconfigured 2FA (enabled, but no secret/method) fails closed with a clear message.
- The HOTP counter is stored in `$EDM_STATE_DIR/hotp_counter` (default `/var/lib/enginedm/hotp_counter`, 0600) together with a fingerprint of the secret. Changing the secret restarts the counter at 0 automatically.
- PAM initialization failures are returned as errors instead of panicking; `acct_mgmt` checks for expired/locked accounts.
- The typed password is held in a zeroizing cell that is wiped right after `authenticate`/`acct_mgmt`; PAM uses a dedicated conversation handler.
- The password is taken from key events (one physical key = one character; a repeated `KeyDown` without a `KeyUp` is ignored). Paste is supported through longer `Input` chunks. For debugging, `EDM_DEBUG_INPUT=1` logs lengths only, never the password.

### Session management

- Sessions start with the target user looked up and privileges dropped (groups/gid/uid), using a clean environment.
- `Exec=` arguments are preserved (e.g. `dbus-run-session Hyprland`).
- `dbus_wrap=` (default `true`): sessions start as `dbus-run-session -- <Exec>`. Skipped when `Exec=` already starts `dbus-run-session`/`dbus-launch`, when `dbus-run-session` is not installed, and on systemd-booted systems. Use `dbus_wrap=false` for custom wrapper scripts.
- Session supervision: spawn, stop, restart and SIGKILL escalation.
- Restart session: see [Restart session](#restart-session-standalone-mode).
- Invalid configuration (`autologin` + `systemctl_usedefine`) returns an error instead of crashing the process.
- The config file is written with mode 0600; `configmanager` shows the secret as `<set>`.
- `hyprctl` commands are split into proper arguments.

### Restart session (standalone mode)

- The `#restart` button (and `enginedmgreeterd --restart-session`) now really restarts the running session. Before, `restart_session` was a stub that always answered "restart is not available".
- After the handover the session is hosted by `enginedmgreeterd --run-handoff`, which owns the PAM session. That process now:
  - installs a `SIGUSR1` handler (before anything else, because the default action would terminate it),
  - writes its pid to `/run/enginedm/session.pid` (0600; `$EDM_SESSION_PID_FILE` overrides; removed when the host exits, and `enginedm-run` also removes a stale one),
  - serves restart requests in `SessionRunner::wait_with_restart`: SIGTERM to the session's process group, SIGKILL after 3 s, then spawn again **inside the same PAM session** (no new login, same logind session).
- `edm_core::restart::request_restart` reads the pid file and sends `SIGUSR1`. It refuses a pid file that is not owned by the caller, is group/world-writable or malformed, and a pid that no longer exists (stale file); it never signals pid 1. The request is asynchronous: the greeter shows "Session restart requested"; the result is logged by the host (`session '...' of <user> restarted` / `could not be restarted: ...`).
- If the respawn fails the session ends and the run loop shows the greeter again.
- `enginedmgreeterd --restart-session` sends the request from a shell (needs the right to signal the host, in practice root).
- Not available under greetd (greetd owns the session); the greeter reports "restart is not available under greetd".
- New unit tests in `edm-core::restart`: pid file round trip and cleanup, refusal of stale/malformed/loose files, signal flag set exactly once, clear error when no host is running.

### greetd mode (recommended)

- Activates automatically when `GREETD_SOCK` is set; `edm-core::greetd` speaks the greetd protocol.
- The greeter checks password + 2FA, sends `create_session` / `start_session`, and exits. PAM, logind, VT and seat stay with greetd, which makes the display handover clean.
- Starting point: `data/greetd/config.toml` (`command = "/usr/libexec/enginedm-weston"`).
- `pam_service=` is ignored in this mode (greetd uses its own `greetd` PAM service); restart session is unavailable.
- The greeter runs as the `greeter` user: it needs write access to `/var/lib/enginedm` and read access to the config (`install.sh` sets 0640 root:greeter when that user exists).
- Starting sessions in this mode does not require root.

### Standalone mode

- Without `GREETD_SOCK`, the greeter hosts the PAM session itself on a supervisor thread.
- If the window closes while the session is running, the greeter waits for the session to end.
- Requires root to start sessions for other users.

### Compositor: Weston kiosk-shell

- `data/common/enginedm-weston` starts `weston --backend=drm --socket=enginedm-greeter --config=/etc/enginedm/weston.ini` (kiosk-shell: fullscreen greeter, no panel, no key bindings).
- It waits for the socket, runs the greeter with `WAYLAND_DISPLAY=enginedm-greeter`, and stops Weston when the greeter exits (SIGTERM, then SIGKILL after 3 s).
- `/etc/conf.d/enginedm` settings:
  - `weston_delay`: seconds to wait once before the first Weston start after boot (default 3, 0 = off)
  - `weston_backend`
  - `weston_args`
  - `weston_config`
- `install.sh --remove` cleans up the installed files.
- Plymouth: `enginedm-run` and `enginedm-weston` run `plymouth quit` and wait for it before starting Weston, so the DRM device is free and there is no black screen or frozen splash. `splash_quit=0` in `/etc/conf.d/enginedm` turns this off.

### systemd (standalone mode)

- `install.sh` offers `/etc/systemd/system/enginedm.service` when `systemctl` exists.
- It uses the same run loop and `/etc/conf.d/enginedm` as the OpenRC service, and the PAM service with `pam_systemd` (`pam_service=enginedm`).
- Requirements: `weston` (kiosk-shell, Weston 10+), `openvt` (package `kbd`).

```sh
sudo systemctl enable enginedm.service     # takes over display-manager.service
sudo systemctl set-default graphical.target
sudo systemctl start enginedm.service      # log: /var/log/enginedm.log
```

- Any other display manager must be disabled first (`systemctl disable gdm` etc.). The unit conflicts with `getty@tty7` (the greeter VT).

### OpenRC (standalone mode)

- `install.sh` detects OpenRC (`openrc-run`) and installs `/etc/init.d/enginedm`, `/etc/conf.d/enginedm` and a PAM service with `pam_elogind` (`data/pam.d/enginedm.elogind`).
- Requirements: `weston` (kiosk-shell, Weston 10+), `openvt` (package `kbd`), and `elogind` or `seatd`.

```sh
# in /etc/enginedm/enginedm.conf: pam_service=enginedm
sudo rc-update add enginedm default
sudo rc-service enginedm start      # log: /var/log/enginedm.log
```

- **Handover:** the greeter logs in, writes `/run/enginedm/handoff` (0600) and exits; `enginedm-weston` then stops Weston. `/usr/libexec/enginedm-run` then starts `enginedmgreeterd --run-handoff` on the freed VT, which opens the PAM session, starts the user's compositor, and shows the greeter again when the session ends. Session output goes to `/var/log/enginedm-session.log`.
- The service runs the greeter as root inside `weston` with `kiosk-shell` on VT 7 (`vt=` in conf.d) under `supervise-daemon`. It gives up after 10 restarts in 60 s.
- The init script orders itself after `localmount` only if `/etc/init.d/localmount` exists (Artix yes, Devuan no).
- greetd users use greetd's own OpenRC service and `/etc/greetd/enginedm-config.toml`.

### Configuration keys

- `interface=`: name of the interface folder (falls back to `default_session=`).
- `pam_service=`: PAM service name (`/etc/pam.d/<name>`); default `system-login`. `install.sh` installs `data/pam.d/enginedm`; use `pam_service=enginedm` to get `pam_systemd`.
- `dbus_wrap=`: see Session management.
- `wallpaper=`: image path (absolute, or relative to the interface directory; png/jpg/webp/gif, max 32 MiB). Read at startup and embedded as a `data:` URI.
- `user_dirs { ... }`: folders scanned by the picture inspector.
- `user_settings { filter=a.png /b.jpg "/c d.webp" }`: `filter=` names the pictures the inspector lists while its search box is empty (in that order; paths separated by spaces, quotes for spaces; absolute, `~`/`$HOME`, or relative to the `user_dirs` folders then `$HOME`; max 500). `filter_known=N` limits that list to the first N entries; without `filter=` it lists the first N pictures alphabetically (max 500, 0 = none). With neither, the list stays empty until you search. Unknown keys are reported and ignored.

### Interface contract

No JavaScript required. The host finds elements **by id** and drives the page:

- required: `#username` `#password` `#login`
- optional: `#twofactor` `#twofactor-row` `#error` `#sessions` `#themes` `#restart` `#password-toggle` `#password-toggle-show` `#password-toggle-hide`
- `#sessions` / `#themes` are filled with `<button class="opt" id="session-N">` (`theme-N`); the chosen one gets class `selected`.
- `#twofactor-row` gets class `hidden` when 2FA is off; `#error` gets `msg error|info visible` and new text.
- Enter in a field, or clicking `#login`, logs in.
- Password show/hide (`#password-toggle`) switches the input's `type` attribute.
- Button lists are used instead of `<select>`.

Interfaces are searched in `$EDM_INTERFACE_DIR`, `/etc/enginedm/interfaces/`, `/usr/local/share/enginedm/interfaces/`, `/usr/share/enginedm/interfaces/` (nothing is searched under `$HOME`). `style.css` next to `main.html` is injected automatically. If a custom interface is missing or invalid, the greeter falls back to the built-in Hyproman, so a broken theme cannot lock you out.

### Picture inspector (Hyproman)

- The eye button (`#inspector-toggle`, top right) opens `#inspector`.
- The host indexes the folders from the `user_dirs { ... }` block of `enginedm.conf` (png/jpg/webp/gif, at most 5000 indexed) and lists them in `#wallpapers` (`<button class="pic" id="pic-N">`).
- The list stays empty until you type in the optional `#wallpaper-search` input; it shows the first 100 matches, every word must occur in the name, case-insensitive.
- `#inspector-info` shows, per label, how many pictures and bytes were indexed, plus entries that could not be used.
- Clicking a picture reads the file as raw bytes (max 12 MB), embeds it as a `data:` URI and sets it as the `style` of `#bg`.

```
user_dirs {
    Pictures=/home/me/Pictures/*     # the folder and everything below it
    Pictures=/home/me/Pictures/sha   # only the pictures directly in this folder
    Walls=$HOME/Walls                # $HOME, ${HOME}, ~ are expanded; the result must be an absolute path
}
```

- The part before `=` is only a label (names the group and its statistics). `Pictures://home/me/Pictures/*` and `file:///...` spellings are accepted and folded to the same thing; the documented form is the plain path.
- `/*` is allowed only as the last part. `..`, a relative path and `/` are refused. A bad line is reported on stderr and skipped; it never blocks the login screen. Without a `user_dirs` block the inspector scans `$EDM_PICTURES_DIR` or `$HOME/Pictures/*`.
- Symlinks are not followed; `/*` goes at most 8 levels deep and looks at no more than 50 000 directory entries per scan.
- The picked picture is remembered by writing its path into `wallpaper=` in `enginedm.conf` (the active line, else the commented `#wallpaper=` example, else a new line; comments, order, owner and mode of the file are kept; no copy of the picture is made). Writing the config needs root: an unprivileged greeter reports "not saved to the config" and keeps the in-memory background.
- `enginedmgreeterd --check` prints the per-label summary (`pictures :`) without opening a window.

### Wallpaper

- A separate cargo feature: `cargo build --release -p edm-greeter --features wallpaper` (adds `blitz-net` + `tokio`).
- The startup `wallpaper=` and inspector pictures are embedded as `data:` URIs and drawn through this net provider; with the feature off, the gradient is used.
- The picture inspector itself does not need this feature.

### Fixes

- Login now authenticates the user in the username field instead of the greeter process's user, and starts the session after login.
- 2FA is active; codes starting with `0` compare correctly; clock-drift tolerance was added.
- Sessions start as the target user instead of the greeter's user.
- `Exec=` arguments are no longer lost.
- The double free in the FFI's `compositor_run_with_ipc` is gone with the removal of the FFI layer; the unused Smithay compositor stub and the very old smithay dependency were dropped.
- The theme manager is created with the correct config path; theme switching works and the configured theme is applied.
- The config file is written 0600 and the 2FA secret is no longer printed.
- `hyprctl` commands are split correctly.
- Doubled password characters were fixed.
- "Restart session" did nothing useful in standalone mode (stub); it now restarts the session through the session host.
- The black screen / frozen splash caused by Plymouth was fixed.
