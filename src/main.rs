// Copyright (C) 2026 Connor Pietrasik
//
// wl-sniper — hold-to-sniper DPI switch daemon for WLmouse.
//
// A mouse button (firmware-bound to a key — default SCROLLLOCK/70, since the
// firmware's key mode only sends F1-F12 plus the web-UI's small key set)
// switches between two DPI stages over the vendor HID command interface.
// The dongle evdev node carrying that key is EVIOCGRABbed at startup, so the
// keypress is consumed silently: it never reaches the compositor or any
// application, and the physical keyboard (a separate evdev device) is
// untouched.
//
// AGPL-3.0-or-later (see LICENSE). HID protocol code derived from
// wl-mouse (AGPL-3.0-or-later), https://heliopolis.live/creations/wl-mouse.

use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;

mod protocol;
use protocol::*;

const EV_KEY: u16 = 1; // input-event-codes.h
const DEFAULT_BUTTON: u16 = 70; // KEY_SCROLLLOCK

#[derive(Parser)]
#[command(name = "wl-sniper", version, about = "Hold-to-sniper DPI switch daemon for WLmouse — see README")]
struct Args {
	/// Mouse-button key to bind, by evdev name (SCROLLLOCK, F12, BTN_EXTRA,
	/// ...) or code (70, 0x46). The mouse button must be bound to this key
	/// in the gm.wlmouse.gg web UI. Default: SCROLLLOCK (70).
	#[arg(long, value_name = "NAME|CODE")]
	button: Option<String>,

	/// DPI stage (1-6) applied while the button is held (required)
	#[arg(long, value_name = "1-6", value_parser = parse_stage)]
	sniper_stage: u8,

	/// DPI stage (1-6) restored on release (required)
	#[arg(long, value_name = "1-6", value_parser = parse_stage)]
	normal_stage: u8,

	/// Profile to modify (default: active profile at start)
	#[arg(long, value_name = "N")]
	profile: Option<u8>,

	/// Only print errors
	#[arg(short, long)]
	quiet: bool,
}

fn main() {
	let args = Args::parse();
	let button = match args.button.as_deref().map(parse_button).transpose() {
		Ok(b) => b.unwrap_or(DEFAULT_BUTTON),
		Err(e) => {
			eprintln!("wl-sniper: {e:#}");
			std::process::exit(2);
		}
	};
	if let Err(e) = run(&args, button) {
		eprintln!("wl-sniper: {e:#}");
		std::process::exit(1);
	}
}

/// `SCROLLLOCK`, `f12`, `BTN_EXTRA`, `70`, `0x46` → evdev code.
fn parse_button(spec: &str) -> Result<u16> {
	if let Ok(d) = spec.parse::<u16>() {
		return Ok(d);
	}
	if let Some(hex) = spec.strip_prefix("0x").or_else(|| spec.strip_prefix("0X"))
		&& let Ok(h) = u16::from_str_radix(hex, 16)
	{
		return Ok(h);
	}
	let up = spec.to_ascii_uppercase();
	if let Some(n) = up.strip_prefix('F') {
		let n: u16 = n.parse().with_context(|| format!("bad F-key name: {spec}"))?;
		// Linux KEY_F* is NOT contiguous: F1..F10 = 59..=68, F11/F12 = 87/88,
		// F13..F24 = 183..=194.
		return Ok(match n {
			1..=10 => 59 + n - 1,    // KEY_F1..KEY_F10
			11 => 87,                // KEY_F11
			12 => 88,                // KEY_F12
			13..=24 => 183 + n - 13, // KEY_F13..KEY_F24
			_ => bail!("unsupported F-key: {spec} (F1-F24)"),
		});
	}
	const KNOWN: &[(&str, u16)] = &[
		("ESC", 1),
		("PAUSE", 19),
		("PRINT", 69),
		("NUMLOCK", 69),
		("SCROLLLOCK", 70),
		("CAPSLOCK", 58),
		("BTN_LEFT", 0x110),
		("BTN_RIGHT", 0x111),
		("BTN_MIDDLE", 0x112),
		("BTN_SIDE", 0x113),
		("BTN_EXTRA", 0x114),
		("BTN_FORWARD", 0x115),
		("BTN_BACK", 0x116),
		("BTN_TASK", 0x117),
	];
	if let Some((_, code)) = KNOWN.iter().find(|(name, _)| *name == up) {
		return Ok(*code);
	}
	bail!(
		"unrecognized button {spec:?} — use an evdev name (SCROLLLOCK, F12, BTN_EXTRA, ...), \
		 a decimal code (70) or hex (0x46)"
	)
}

fn parse_stage(s: &str) -> Result<u8, String> {
	let v: u8 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
	if !(1..=6).contains(&v) {
		return Err(format!("{v} not in 1..=6"));
	}
	Ok(v)
}

fn key_name(code: u16) -> String {
	if (59..=68).contains(&code) {
		return format!("F{}", code - 58);
	}
	if (87..=88).contains(&code) {
		return format!("F{}", code - 76);
	}
	if (183..=194).contains(&code) {
		return format!("F{}", code - 182);
	}
	let names: &[(&str, u16)] = &[
		("ESC", 1),
		("PAUSE", 19),
		("PRINT/NUMLOCK", 69),
		("SCROLLLOCK", 70),
		("CAPSLOCK", 58),
		("BTN_LEFT", 0x110),
		("BTN_RIGHT", 0x111),
		("BTN_MIDDLE", 0x112),
		("BTN_SIDE", 0x113),
		("BTN_EXTRA", 0x114),
		("BTN_FORWARD", 0x115),
		("BTN_BACK", 0x116),
		("BTN_TASK", 0x117),
	];
	names
		.iter()
		.find(|(_, c)| *c == code)
		.map(|(n, _)| (*n).to_string())
		.unwrap_or_else(|| format!("{code}"))
}

fn epoch_secs() -> f64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Resolve a /dev node to its canonical sysfs path. /dev/input/eventN and
/// /dev/hidrawN are real character devices, NOT symlinks — canonicalizing
/// them returns the /dev path unchanged. The class symlinks are what point
/// into /sys, so go through those.
fn sysfs_canon(dev_path: &str) -> Option<std::path::PathBuf> {
	let class = if dev_path.starts_with("/dev/input/") {
		"/sys/class/input"
	} else if dev_path.starts_with("/dev/hidraw") {
		"/sys/class/hidraw"
	} else {
		return std::fs::canonicalize(dev_path).ok();
	};
	let name = dev_path.rsplit('/').next()?;
	std::fs::canonicalize(std::path::Path::new(class).join(name)).ok()
}

/// Scope for finding the dongle's evdev nodes. Primary: the physical USB
/// device path (e.g. /sys/devices/.../usb5/5-2) shared by all interfaces of
/// the dongle we opened — node numbers and even PIDs may vary, the USB port
/// path does not. Fallback: the VID:PID marker in the sysfs path (the parent
/// HID device dir is named `0003:36A7:A863.NNNN`).
struct Scope {
	usb_prefix: Option<String>,
	vidpid_marker: String,
}

fn dongle_scope(vendor_path: &str, pid: u16) -> Scope {
	let usb_prefix = sysfs_canon(vendor_path).and_then(|p| extract_usb_device_prefix(&p.to_string_lossy()));
	Scope {
		usb_prefix,
		vidpid_marker: format!("{WL_VID:04x}:{pid:04x}"),
	}
}

/// `/sys/devices/.../usb5/5-2/5-2:1.4/0003:36A7:A863.0004/hidraw3`
/// → `/sys/devices/.../usb5/5-2` (the USB device, interface segment removed).
fn extract_usb_device_prefix(canon: &str) -> Option<String> {
	let segs: Vec<&str> = canon.split('/').filter(|s| !s.is_empty()).collect();
	for i in 1..segs.len() {
		if usb_segment(segs[i], true) && usb_segment(segs[i - 1], false) {
			return Some(format!("/{}", segs[..i].join("/")));
		}
	}
	None
}

/// USB device segment `5-2` (hub ports chain with dots: `3-1.2`) or
/// interface segment `5-2:1.3` (config.interface).
fn usb_segment(seg: &str, with_iface: bool) -> bool {
	let (dev, iface) = match seg.split_once(':') {
		Some((d, i)) => (d, Some(i)),
		None => (seg, None),
	};
	let port_ok = dev.split('-').count() >= 2
		&& dev
			.split('-')
			.all(|p| !p.is_empty() && p.split('.').all(|q| !q.is_empty() && q.bytes().all(|b| b.is_ascii_digit())));
	if with_iface {
		iface.is_some_and(|i| {
			let parts: Vec<&str> = i.split('.').collect();
			parts.len() == 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
		}) && port_ok
	} else {
		iface.is_none() && port_ok
	}
}

/// Dongle evdev node paths from sysfs alone (no open required), for error
/// messages: evdev::enumerate() silently skips devices we cannot open.
fn dongle_event_nodes_from_sysfs(marker: &str) -> Vec<String> {
	let mut out = Vec::new();
	let Ok(rd) = std::fs::read_dir("/sys/class/input") else {
		return out;
	};
	for e in rd.flatten() {
		let name = e.file_name().to_string_lossy().into_owned();
		if !name.starts_with("event") {
			continue;
		}
		let Ok(real) = std::fs::canonicalize(e.path()) else {
			continue;
		};
		if real.to_string_lossy().to_lowercase().contains(marker) {
			out.push(format!("/dev/input/{name}"));
		}
	}
	out
}

fn in_scope(canon_lower: &str, scope: &Scope) -> bool {
	match &scope.usb_prefix {
		Some(prefix) => canon_lower.contains(&prefix.to_lowercase()),
		None => canon_lower.contains(&scope.vidpid_marker.to_lowercase()),
	}
}

fn run(args: &Args, button: u16) -> Result<()> {
	// 1. The single vendor command interface (usage_page 0xffff / usage 0).
	let api = hidapi::HidApi::new().context("hidapi init failed")?;
	let mut vendor_nodes: Vec<(String, u16)> = Vec::new();
	for info in api.device_list() {
		if info.vendor_id() != WL_VID {
			continue;
		}
		if info.usage_page() == 0xFFFF && info.usage() == 0 {
			let path = info.path().to_string_lossy().into_owned();
			vendor_nodes.push((path, info.product_id()));
		}
	}
	let (vendor_path, pid) = match vendor_nodes.len() {
		0 => bail!(
			"no WLmouse vendor command interface found (VID 0x{WL_VID:04x}, usage_page 0xffff/usage 0). \
			 Is the dongle connected? (check: lsusb | grep -i 36a7)"
		),
		1 => vendor_nodes.into_iter().next().unwrap(),
		n => {
			let list: Vec<String> = vendor_nodes.iter().map(|(p, p2)| format!("{p} (0x{p2:04x})")).collect();
			bail!(
				"{n} WLmouse vendor command interfaces found ({}) — exactly one is required",
				list.join(", ")
			)
		}
	};
	let model = KNOWN_PIDS
		.iter()
		.find(|(p, _)| *p == pid)
		.map(|(_, n)| *n)
		.unwrap_or("unknown model");

	let cpath = std::ffi::CString::new(vendor_path.as_str()).with_context(|| format!("bad vendor path {vendor_path}"))?;
	let hid = api.open_path(&cpath).with_context(|| {
		format!(
			"could not open {vendor_path} (no /dev/hidraw* in a container, or missing permissions — input group + udev rule, see README)"
		)
	})?;

	let mut transport = HidTransport::new(&hid);
	transport
		.detect_hid_index()
		.context("mouse did not answer the vendor interface (asleep or out of range? wiggle it)")?;

	let profile = match args.profile {
		Some(p) => p,
		None => {
			let resp = transport.send_and_recv(&build_get_profile_id()).context("get_profile_id failed")?;
			resp[7 - transport.hid_index as usize]
		}
	};

	// 2. The single dongle evdev node that advertises the button key.
	let scope = dongle_scope(&vendor_path, pid);
	let mut matchers: Vec<(String, String)> = Vec::new();
	let mut dongle_nodes: Vec<String> = Vec::new();
	for (path, dev) in evdev::enumerate() {
		let p = path.to_string_lossy().into_owned();
		let canon = sysfs_canon(&p)
			.map_or(String::new(), |c| c.to_string_lossy().into_owned())
			.to_lowercase();
		if !in_scope(&canon, &scope) {
			continue;
		}
		let name = dev.name().unwrap_or("?").to_string();
		dongle_nodes.push(format!("{p} (\"{name}\")"));
		let has_button = dev.supported_keys().is_some_and(|k| k.contains(evdev::KeyCode::new(button)));
		if has_button {
			matchers.push((p, name));
		}
	}
	match matchers.len() {
		0 => {
			// enumerate() silently skips devices we cannot open, so an empty
			// dongle_nodes list is ambiguous. Disambiguate with a sysfs scan:
			// "exists but unopenable" (permissions) vs "does not exist".
			let sysfs_nodes = dongle_event_nodes_from_sysfs(&scope.vidpid_marker);
			let mut seen = if dongle_nodes.is_empty() {
				"none openable".to_string()
			} else {
				format!("{} (none advertises {})", dongle_nodes.join(", "), key_name(button))
			};
			if dongle_nodes.is_empty() && !sysfs_nodes.is_empty() {
				seen = format!(
					"{seen} — but sysfs lists dongle nodes {} (not openable by you: are you in the 'input' group? full re-login after usermod; or run with sudo to test)",
					sysfs_nodes.join(", ")
				);
			} else if dongle_nodes.is_empty() {
				seen = format!("{seen} (and sysfs lists no dongle evdev nodes at all — dongle disconnected?)");
			}
			bail!(
				"no dongle evdev node advertises key {} ({}). Is the mouse button bound to that key \
				 in the gm.wlmouse.gg web UI? Dongle nodes seen: {seen}",
				key_name(button),
				button
			)
		}
		1 => {}
		n => {
			let list: Vec<&str> = matchers.iter().map(|(p, _)| p.as_str()).collect();
			bail!(
				"{n} dongle nodes advertise key {} ({button}): {} — ambiguous, cannot proceed",
				key_name(button),
				list.join(", ")
			)
		}
	}
	let (node_path, node_name) = matchers.into_iter().next().unwrap();

	// 3. Grab it: from now on the kernel delivers this node's events only to
	//    us, so the key is silently consumed. Tied to the fd — any exit path
	//    (incl. SIGKILL) auto-releases it.
	let mut dev = evdev::Device::open(&node_path)
		.with_context(|| format!("could not open {node_path} — permission? (are you in the 'input' group? full re-login after usermod)"))?;
	if dev.is_grabbed() {
		bail!("{node_path} is already grabbed — is another wl-sniper already running?");
	}
	dev.grab()
		.with_context(|| format!("could not grab {node_path} (another process may have grabbed it in between)"))?;

	// Normalize the baseline: the dongle may be sitting on any stage,
	// including an unconfigured one (the active-stage readout is known to
	// be unreliable). Setting the normal stage once at startup guarantees
	// a known DPI; it is idempotent and identical to what the first
	// release would send. A failure is non-fatal — the next edge retries.
	if let Err(e) = HidTransport::new(&hid).send_only(&build_set_active_dpi(profile, args.normal_stage)) {
		eprintln!(
			"wl-sniper: warning: initial set stage {} failed: {e} (keeps running; next edge retries)",
			args.normal_stage
		);
	}

	if !args.quiet {
		println!(
			"wl-sniper: {model} (0x{pid:04x}) {vendor_path} · {} ({button}) {node_path} \"{node_name}\" (grabbed) · stages {}→{}, profile {} — Ctrl-C to stop",
			key_name(button),
			args.normal_stage,
			args.sniper_stage,
			profile
		);
	}

	// 4. Event loop. Non-blocking 2 ms poll: fast enough for a DPI trigger,
	//    and it drains the kernel queue on the grabbed node (a grabbed node
	//    whose events are not read can drop them after ~30 slots).
	dev.set_nonblocking(true)
		.with_context(|| format!("set_nonblocking on {node_path}"))?;
	let mut last_read_err = Instant::now() - Duration::from_secs(10);
	loop {
		match dev.fetch_events() {
			Ok(events) => {
				for ev in events {
					if ev.event_type().0 != EV_KEY || ev.code() != button {
						continue;
					}
					let stage = match ev.value() {
						1 => Some(args.sniper_stage),
						0 => Some(args.normal_stage),
						2 => None, // auto-repeat: none observed, ignore anyway
						v => {
							eprintln!(
								"wl-sniper: [t={:.2}] unexpected EV_KEY value {v} for {} — ignored",
								epoch_secs(),
								key_name(button)
							);
							continue;
						}
					};
					if let Some(stage) = stage {
						match HidTransport::new(&hid).send_only(&build_set_active_dpi(profile, stage)) {
							Ok(()) => {
								if !args.quiet {
									println!(
										"wl-sniper: [t={:.2}] {} -> stage {stage}",
										epoch_secs(),
										if stage == args.sniper_stage { "press" } else { "release" }
									);
								}
							}
							Err(e) => eprintln!(
								"wl-sniper: [t={:.2}] warning: set stage {stage} failed: {e} (keeps running; next edge retries)",
								epoch_secs()
							),
						}
					}
				}
			}
			Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
			Err(e) => {
				// E.g. dongle unplugged. Keep polling (it may be replugged);
				// rate-limit the warning.
				if last_read_err.elapsed() > Duration::from_secs(5) {
					eprintln!(
						"wl-sniper: [t={:.2}] warning: read {node_path} failed: {e} (keeps running)",
						epoch_secs()
					);
					last_read_err = Instant::now();
				}
			}
		}
		std::thread::sleep(Duration::from_millis(2));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn usb_segments() {
		// device segments
		assert!(usb_segment("5-2", false));
		assert!(usb_segment("3-1.2", false)); // hub port
		assert!(usb_segment("1-1.1.3", false)); // nested hub ports
		assert!(!usb_segment("usb5", false));
		assert!(!usb_segment("5-2:1.3", false));
		// interface segments (config.interface)
		assert!(usb_segment("5-2:1.3", true));
		assert!(usb_segment("3-1.2:2.0", true));
		assert!(!usb_segment("5-2", true));
		assert!(!usb_segment("5-2:1", true)); // bare interface is not USB sysfs format
		assert!(!usb_segment("5-2:1.3.4", true));
	}

	#[test]
	fn extract_prefix_dongle_paths() {
		let p = "/sys/devices/pci0000:00/0000:00:14.0/usb5/5-2/5-2:1.3/0003:36A7:A863.0005/hidraw3";
		assert_eq!(
			extract_usb_device_prefix(p),
			Some("/sys/devices/pci0000:00/0000:00:14.0/usb5/5-2".to_string())
		);
		let evdev_p = "/sys/devices/pci0000:00/0000:00:14.0/usb5/5-2/5-2:1.3/0003:36A7:A863.0005/input7/event6";
		assert_eq!(
			extract_usb_device_prefix(evdev_p),
			Some("/sys/devices/pci0000:00/0000:00:14.0/usb5/5-2".to_string())
		);
	}

	#[test]
	fn extract_prefix_real_host_paths() {
		// Real sysfs paths from the reporter's host (CachyOS, 7.2.0 kernel).
		let vendor = "/sys/devices/pci0000:00/0000:00:08.1/0000:74:00.4/usb5/5-2/5-2:1.2/0003:36A7:A863.0004/hidraw/hidraw3";
		let evdev = "/sys/devices/pci0000:00/0000:00:08.1/0000:74:00.4/usb5/5-2/5-2:1.1/0003:36A7:A863.0002/input/input5/event6";
		let want = "/sys/devices/pci0000:00/0000:00:08.1/0000:74:00.4/usb5/5-2";
		assert_eq!(extract_usb_device_prefix(vendor), Some(want.to_string()));
		assert_eq!(extract_usb_device_prefix(evdev), Some(want.to_string()));
		// Fallback marker also matches both.
		let marker = "36a7:a863";
		assert!(vendor.to_lowercase().contains(marker));
		assert!(evdev.to_lowercase().contains(marker));
		// A different 36A7 model (wired, 0xa864) must NOT match the dongle marker.
		assert!(
			!"/sys/devices/x/usb1/1-1/1-1:1.0/0003:36A7:A864.0009/input/input9/event9"
				.to_lowercase()
				.contains(marker)
		);
	}

	#[test]
	fn extract_prefix_rejects_non_usb() {
		// A /dev path (real device node, not a symlink) has no USB segments.
		assert_eq!(extract_usb_device_prefix("/dev/hidraw3"), None);
		assert_eq!(extract_usb_device_prefix("/dev/input/event6"), None);
	}

	#[test]
	fn parse_button_fkeys_match_evdev() {
		// Linux KEY_F* is not contiguous: F10=68, F11=87, F12=88, F13..F24=183..=194.
		assert_eq!(parse_button("F1").unwrap(), 59);
		assert_eq!(parse_button("F10").unwrap(), 68);
		assert_eq!(parse_button("F11").unwrap(), 87);
		assert_eq!(parse_button("F12").unwrap(), 88);
		assert_eq!(parse_button("f24").unwrap(), 194);
	}

	#[test]
	fn parse_button_names_and_codes() {
		assert_eq!(parse_button("SCROLLLOCK").unwrap(), 70);
		assert_eq!(parse_button("scrolllock").unwrap(), 70);
		assert_eq!(parse_button("70").unwrap(), 70);
		assert_eq!(parse_button("0x46").unwrap(), 70);
		assert_eq!(parse_button("BTN_EXTRA").unwrap(), 0x114);
		assert!(parse_button("F25").is_err());
		assert!(parse_button("nope").is_err());
	}
}
