# wl-sniper

Hold-to-sniper DPI switch for **WLmouse** mice (8K 2.4 GHz dongle,
`36A7:A863` et al.). Hold a mouse button → the mouse switches to a low-DPI
"sniper" stage; release → your normal stage. No keyboard involved, no
virtual keys, zero latency impact on the mouse itself.

- AGPL-3.0-or-later
- HID protocol code derived from [wl-mouse] (AGPL-3.0-or-later),
  https://heliopolis.live/creations/wl-mouse

[wl-mouse]: https://heliopolis.live/creations/wl-mouse

## How it works

1. You bind a mouse button to a **key** in the gm.wlmouse.gg web UI.
   **Default: `SCROLL LOCK`** (see "Firmware binding" below for why).
2. At startup `wl-sniper` finds the dongle's vendor command interface
   (`hidraw`, usage page `0xffff`/usage `0`) and the single dongle evdev
   node that carries your key, and issues **`EVIOCGRAB`** on it. From that
   moment the kernel delivers that node's events *only* to wl-sniper — your
   mouse's Scroll Lock press is silently consumed: it never reaches the
   compositor or any application, and it does **not** toggle the system
   scroll-lock state.
3. On press wl-sniper sends the vendor "set active DPI stage" command with
   your sniper stage (fire-and-forget, ~0 ms); on release it sends the
   normal stage. Stateless and idempotent: a dropped write self-heals on
   the next edge.

Because the grab is per-node, the 8K mouse interface (motion, wheel, real
buttons) is untouched — zero added latency anywhere. The grab is tied to
the file descriptor: killing wl-sniper (even `kill -9`) auto-releases it,
so the key simply works again — the failure mode is "button behaves like a
normal key", never a stuck device.

**Grab semantics (accepted trade-off):** *all* keycodes on that dongle
keyboard-interface node are consumed, not just yours. Any other
button→key mapping you make in the web UI is swallowed too. If you ever
need another mouse-button key to actually reach apps, bind that button as
a raw `BTN_*` mouse button instead (raw buttons ride the 8K mouse
interface, which is never grabbed).

## Firmware binding

Bind your button to **`SCROLL LOCK`** (web UI → button → keyboard mode).
Requirements:

- **Key mode** — never the firmware's own "DPI toggle" (the event is
  swallowed by the firmware, there is nothing for the daemon to see), and
  never a raw button (a raw button is visible to apps, and consuming it
  would require uinput replay over the 8K node — the latency risk this
  design avoids).
- **`SCROLL LOCK` or similar** (`PAUSE`, `PRINT` also work). Why not an
  F-key? **The firmware's key mode only emits the web UI's key set:
  F1–F12.** F13–F24 *can be stored* in the button config (verified:
  written over the vendor protocol and read back), but the mouse never
  transmits them — confirmed 2026-09-13 with a raw evdev tail
  (F12 works, F13/F24 produce nothing). Scroll Lock is in the web UI's
  key map natively, so no capture workarounds are needed.
- Pick a key you don't need from your **physical** keyboard's perspective:
  the mouse's press is fully consumed, but the real key still does what
  it normally does (e.g. toggling scroll lock).

The two DPI stages (e.g. stage 1 = 500×500 sniper, stage 2 = 1000×1000
normal) are set for the profile in the same web app.

## Install (CachyOS/Arch + KDE Plasma shown)

```sh
# 1. Permissions: /dev/input/event* and the dongle's /dev/hidraw* must be
#    readable by your user.
groups                                   # must include "input"
# (if not: sudo usermod -aG input <user>, then a FULL re-login)
sudo tee /etc/udev/rules.d/99-wlmouse.rules >/dev/null <<'EOF'
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="36a7", GROUP="input", MODE="0660"
EOF
sudo udevadm control --reload-rules
# replug the dongle (or: sudo udevadm trigger --subsystem-match=hidraw)
ls -l /dev/hidraw*                        # 36a7 nodes must be root:input 0660

# 2. The binary (x86_64 glibc build in bin/), or build from source:
#    cargo build --release  (deps: hidapi, evdev, clap, anyhow) — produces
#    both wl-sniper and the companion wl-probe (see "Diagnosing")
install -Dm755 bin/wl-sniper-x86_64-linux ~/.local/bin/wl-sniper
install -Dm755 bin/wl-probe-x86_64-linux ~/.local/bin/wl-probe
```

## Diagnosing: wl-probe

The package builds a second, strictly **read-only** companion binary,
`wl-probe` (source: `src/bin/wl-probe.rs`): no `EVIOCGRAB`, no HID feature
writes, no event injection. If wl-sniper refuses to start — or before
changing button bindings in the web UI — run it to see, in one screen,
whether the current user can open everything wl-sniper needs:

```sh
./bin/wl-probe-x86_64-linux            # defaults to SCROLLLOCK
./bin/wl-probe-x86_64-linux F12        # or the key you actually bound
./bin/wl-probe-x86_64-linux --tail     # + live EV_KEY tail: press the button,
                                       #   see which node emits which key (Ctrl-C to stop)
```

Note for `--tail`: if wl-sniper is currently running it has `EVIOCGRAB`bed
the key node, so the tail will see nothing on that node — stop wl-sniper
first when checking bindings.

Per node it shows: sysfs path, HID VID:PID, name, openability, grabbed
state and advertised keys; for the WLmouse vendor interface whether it's
openable read+write; and a verdict on whether wl-sniper can start.

| verdict line | meaning / fix |
|---|---|
| `✗ ... NOT read+write openable` | udev rule missing (see Install) or not in `input` group |
| `✗ none openable` | join `input` group + **full re-login** (`sudo usermod -aG input $USER`) |
| `✗ no openable dongle node advertises key ...` | the button isn't bound to that key in the web UI (key mode); the probe lists every key the dongle's nodes actually advertise — pick one and re-run with it |
| `✓ wl-sniper should start: it will grab ...` | go run wl-sniper |

## Usage

```
wl-sniper [OPTIONS] --sniper-stage <1-6> --normal-stage <1-6>

Options:
      --button <NAME|CODE>   Key bound to the mouse button in the web UI.
                             Name (SCROLLLOCK, F12, BTN_EXTRA, ...) or code
                             (70, 0x46). Default: SCROLLLOCK (70)
      --sniper-stage <1-6>   DPI stage while held (required)
      --normal-stage <1-6>   Stage restored on release (required — the
                             dongle's active-stage readout is unreliable,
                             so pass both explicitly)
      --profile <N>          Profile to modify (default: active at start)
  -q, --quiet                Only print errors
  -h, --help
  -V, --version
```

Typical:

```sh
wl-sniper --sniper-stage 1 --normal-stage 2
# wl-sniper: HUAN (dongle) (0xa863) /dev/hidraw3 · SCROLLLOCK (70)
#   /dev/input/event6 "HUAN 8K RECEIVER" (grabbed) · stages 2→1, profile 1
#   — Ctrl-C to stop
```

Notes:

- `--button` must match the web UI binding, and only the **dongle's**
  evdev nodes are considered — a physical-keyboard Scroll Lock is never
  consumed.
- If the node is already grabbed you get:
  `is another wl-sniper already running?`
- Press → one log line per edge (unless `-q`); write warnings are
  timestamped and non-fatal (the next edge retries).
- Ctrl-C / any death releases the grab automatically.

## Autostart (systemd user unit)

`~/.config/systemd/user/wl-sniper.service`:

```ini
[Unit]
Description=wl-sniper DPI button daemon

[Service]
ExecStart=%h/.local/bin/wl-sniper --button 70 --sniper-stage 1 --normal-stage 2
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now wl-sniper
journalctl --user -u wl-sniper -f
```
