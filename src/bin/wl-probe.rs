// Copyright (C) 2026 Connor Pietrasik
//
// wl-probe — read-only capability probe for wl-sniper (WLmouse 8K dongle).
// Companion binary of the wl-sniper package (src/bin/wl-probe.rs).
//
// Answers, in one run, the questions wl-sniper's startup needs:
//   1. Can the current user open the dongle's vendor hidraw node (read+write)?
//   2. Which evdev nodes belong to the dongle, and can they be opened?
//   3. Which of them advertises the target button key?
//   4. Therefore: will wl-sniper start, and which node will it grab?
//
// Strictly read-only: no EVIOCGRAB, no HID feature writes, no event
// injection. Opening a device (to query capabilities) is side-effect free.
//
// Usage:
//   wl-probe [BUTTON] [--tail]
//   BUTTON = evdev name (SCROLLLOCK, F12, BTN_EXTRA, ...) or code (70, 0x46).
//            Default: SCROLLLOCK (70).
//   --tail   after the static report, tail EV_KEY events from the dongle's
//            nodes: press the sniper button and see which node emits which
//            key. Ctrl-C to stop.
//
// AGPL-3.0-or-later (see LICENSE).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

const WL_VID: u16 = 0x36A7;
const EV_KEY: u16 = 1; // input-event-codes.h
const DEFAULT_BUTTON: u16 = 70; // KEY_SCROLLLOCK

#[allow(dead_code)]
struct Env {
	user: String,
	uid: u32,
	euid: u32,
	groups: Vec<String>,
}

struct InputNode {
	path: String,
	sysfs: Option<String>,
	vidpid: Option<String>, // lowercase "36a7:a863"
	name: Option<String>,
	open_err: Option<String>,
	grabbed: bool,
	ev: Vec<u16>,
	keys: Vec<u16>,
	rel: Vec<u16>,
	abs: Vec<u16>,
}

struct HidrawNode {
	path: String,
	vid: u16,
	pid: u16,
	name: String,
	usages: BTreeSet<(u16, u16)>,
	sysfs: Option<String>,
	ro_open: bool,
	rw_open: bool,
}

fn main() {
	let mut button = DEFAULT_BUTTON;
	let mut help = false;
	let mut tail = false;
	for a in std::env::args().skip(1) {
		match a.as_str() {
			"-h" | "--help" => help = true,
			"--tail" => tail = true,
			spec => {
				button = match parse_button(spec) {
					Ok(b) => b,
					Err(e) => {
						eprintln!("wl-probe: {e}");
						std::process::exit(2);
					}
				};
			}
		}
	}
	if help {
		println!(
			"usage: wl-probe [BUTTON] [--tail] | -h\n\n\
			 Read-only capability probe for wl-sniper. Never writes to any device.\n\n\
			 BUTTON  evdev key name (SCROLLLOCK, F12, BTN_EXTRA, ...) or code (70, 0x46).\n\
			         Default: SCROLLLOCK (70).\n\
			 --tail  after the static report, tail EV_KEY events from the dongle's\n\
			         nodes: press the sniper button and see which node emits which\n\
			         key. Ctrl-C to stop."
		);
		return;
	}

	let env = section_env();
	let (input, dongle_paths) = section_input(button);
	let hidraw = section_hidraw();
	verdict(&env, &input, &dongle_paths, &hidraw, button);

	if tail {
		let vid_marker = format!("{WL_VID:04x}");
		let nodes: Vec<String> = input
			.iter()
			.filter(|i| i.open_err.is_none() && i.vidpid.as_deref().is_some_and(|m| m.starts_with(&vid_marker)))
			.map(|i| i.path.clone())
			.collect();
		section_tail(&nodes);
	}
}

// ---------------------------------------------------------------- live tail

/// Tail EV_KEY events from the dongle's openable nodes (wl-diag's tail, folded
/// in). Opens nodes read-only without grabbing, so it can run alongside the
/// compositor — but if wl-sniper is currently running it has EVIOCGRABbed the
/// key node, and the kernel then delivers that node's events only to
/// wl-sniper: expect silence on that node until wl-sniper is stopped.
fn section_tail(nodes: &[String]) {
	println!("\n== live EV_KEY tail ==");
	if nodes.is_empty() {
		println!("  no openable dongle evdev nodes — nothing to watch (fix the openability problem from the verdict above first)");
		return;
	}
	println!("  watching: {}", nodes.join(", "));
	println!("  press the sniper button now (and the other buttons, to compare). Ctrl-C to stop.");
	std::io::stdout().flush().ok();

	let (tx, rx) = mpsc::channel::<(String, u16, i32)>();
	let t0 = Instant::now();
	for node in nodes {
		let node = node.clone();
		let tx = tx.clone();
		std::thread::spawn(move || {
			let mut dev = match evdev::Device::open(&node) {
				Ok(d) => d,
				Err(e) => {
					eprintln!("  {node}: failed to open for tail: {e}");
					return;
				}
			};
			loop {
				match dev.fetch_events() {
					Ok(events) => {
						for ev in events {
							if ev.event_type().0 == EV_KEY {
								let _ = tx.send((node.clone(), ev.code(), ev.value()));
							}
						}
					}
					Err(e) => {
						eprintln!("  {node}: read error: {e}");
						break;
					}
				}
			}
		});
	}
	drop(tx);

	for (label, code, value) in rx {
		let dt = t0.elapsed().as_secs_f64();
		let verb = match value {
			1 => "press  ",
			0 => "release",
			2 => "repeat ",
			v => {
				eprintln!("  unexpected EV_KEY value {v}");
				continue;
			}
		};
		let label = label.strip_prefix("/dev/input/").unwrap_or(&label);
		println!("  t+{dt:7.3}s  {label:<8}  {verb}  {} ({code}/0x{code:x})", key_name(code));
		std::io::stdout().flush().ok();
	}
}

// ---------------------------------------------------------------- env

fn section_env() -> Env {
	println!("\n== whoami / permissions ==");
	let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
	let (mut uid, mut euid, mut gids): (u32, u32, Vec<u32>) = (0, 0, Vec::new());
	for line in status.lines() {
		if let Some(v) = line.strip_prefix("Uid:") {
			let p: Vec<u32> = v.split_whitespace().filter_map(|t| t.parse().ok()).collect();
			if p.len() >= 2 {
				uid = p[0];
				euid = p[1];
			}
		} else if let Some(v) = line.strip_prefix("Supplementary groups:") {
			gids = v.split_whitespace().filter_map(|t| t.parse().ok()).collect();
		}
	}
	let gid2name: BTreeMap<u32, String> = std::fs::read_to_string("/etc/group")
		.ok()
		.map(|s| {
			s.lines()
				.filter_map(|l| {
					let mut it = l.split(':');
					let name = it.next()?.to_string();
					let gid = it.nth(2)?.parse().ok()?;
					Some((gid, name))
				})
				.collect()
		})
		.unwrap_or_default();
	let groups: Vec<String> = gids
		.iter()
		.map(|g| gid2name.get(g).cloned().unwrap_or_else(|| format!("gid{g}")))
		.collect();
	let user = std::env::var("USER")
		.or_else(|_| std::env::var("LOGNAME"))
		.unwrap_or_else(|_| "?".into());
	let in_input = euid == 0 || groups.iter().any(|g| g == "input");
	println!("  user: {user}  uid: {uid}  euid: {euid}");
	println!("  groups: {}", groups.join(" "));
	if in_input {
		println!("  input group: YES (evdev + hidraw nodes should be openable)");
	} else {
		println!("  input group: NO — wl-sniper will not be able to open /dev/input/* or /dev/hidraw*");
	}
	Env { user, uid, euid, groups }
}

// ---------------------------------------------------------------- /dev/input

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
	std::fs::canonicalize(Path::new(class).join(name)).ok()
}

/// `0003:36A7:A863.0005` segment in a sysfs path → `36a7:a863` (lowercase).
fn hid_marker(sysfs_path: &str) -> Option<String> {
	for seg in sysfs_path.split('/').filter(|s| !s.is_empty()) {
		let s = seg.to_ascii_lowercase();
		if let Some(rest) = s.strip_prefix("0003:") {
			if let Some((vidpid, _inst)) = rest.split_once('.') {
				let parts: Vec<&str> = vidpid.split(':').collect();
				if parts.len() == 2 {
					return Some(format!("{}:{}", parts[0], parts[1]));
				}
			}
		}
	}
	None
}

fn section_input(button: u16) -> (Vec<InputNode>, BTreeSet<String>) {
	println!("\n== /dev/input nodes ==");
	let mut nodes: Vec<(u32, String)> = Vec::new();
	if let Ok(rd) = std::fs::read_dir("/dev/input") {
		for e in rd.flatten() {
			let n = e.file_name().to_string_lossy().into_owned();
			if let Some(num) = n.strip_prefix("event").and_then(|s| s.parse::<u32>().ok()) {
				nodes.push((num, format!("/dev/input/{n}")));
			}
		}
	}
	nodes.sort_by_key(|(n, _)| *n);

	let vid_marker = format!("{WL_VID:04x}");
	let mut rows: Vec<InputNode> = Vec::new();
	let mut dongle_paths: BTreeSet<String> = BTreeSet::new();

	for (_, path) in nodes {
		let mut r = InputNode {
			path: path.clone(),
			sysfs: None,
			vidpid: None,
			name: None,
			open_err: None,
			grabbed: false,
			ev: Vec::new(),
			keys: Vec::new(),
			rel: Vec::new(),
			abs: Vec::new(),
		};
		if let Some(c) = sysfs_canon(&path) {
			let s = c.to_string_lossy().into_owned();
			if let Some(m) = hid_marker(&s) {
				r.vidpid = Some(m.clone());
				if m.starts_with(&vid_marker) {
					dongle_paths.insert(path.clone());
				}
			}
			r.sysfs = Some(s);
		}
		match evdev::Device::open(&path) {
			Ok(dev) => {
				r.name = dev.name().map(|s| s.to_string());
				r.grabbed = dev.is_grabbed();
				r.ev = dev.supported_events().iter().map(|t| t.0).collect();
				r.keys = dev.supported_keys().map(|k| k.iter().map(|c| c.0).collect()).unwrap_or_default();
				r.rel = dev
					.supported_relative_axes()
					.map(|k| k.iter().map(|c| c.0).collect())
					.unwrap_or_default();
				r.abs = dev
					.supported_absolute_axes()
					.map(|k| k.iter().map(|c| c.0).collect())
					.unwrap_or_default();
			}
			Err(e) => r.open_err = Some(e.to_string()),
		}
		rows.push(r);
	}

	for r in &rows {
		let label = r.path.strip_prefix("/dev/input/").unwrap_or(&r.path);
		let is_dongle = r.vidpid.as_deref().is_some_and(|m| m.starts_with(&vid_marker));
		let tag = if is_dongle { " [36A7 dongle]" } else { "" };
		if let Some(e) = &r.open_err {
			println!("  {label:<8} OPEN FAILED ({e}){tag}");
			if let Some(s) = &r.sysfs {
				println!("           sysfs: {}", shorten(s));
			}
			continue;
		}
		let grab = if r.grabbed { "grabbed" } else { "" };
		let adv = if r.keys.contains(&button) {
			format!("  advertises {} ({button})", key_name(button))
		} else {
			String::new()
		};
		println!("  {label:<8} ok{tag}{grab}{adv}");
		if let Some(s) = &r.sysfs {
			println!("           sysfs: {}", shorten(s));
		}
		if let Some(n) = &r.name {
			println!("           name:  \"{n}\"");
		}
		let evs: Vec<&str> = r.ev.iter().map(|t| ev_name(*t)).collect();
		let keys = if r.keys.is_empty() {
			"(none)".to_string()
		} else {
			r.keys
				.iter()
				.map(|c| format!("{}({c})", key_name(*c)))
				.collect::<Vec<_>>()
				.join(" ")
		};
		let rel = if r.rel.is_empty() {
			"".to_string()
		} else {
			format!("  rel: {}", r.rel.iter().map(|c| rel_name(*c)).collect::<Vec<_>>().join(","))
		};
		let abs = if r.abs.is_empty() {
			"".to_string()
		} else {
			format!("  abs: {}", r.abs.iter().map(|c| abs_name(*c)).collect::<Vec<_>>().join(","))
		};
		println!("           EV: {}{rel}{abs}", evs.join(" "));
		println!("           keys: {keys}");
	}
	(rows, dongle_paths)
}

// ---------------------------------------------------------------- /dev/hidraw

fn section_hidraw() -> Vec<HidrawNode> {
	println!("\n== /dev/hidraw nodes ==");
	let mut by_path: BTreeMap<String, HidrawNode> = BTreeMap::new();
	match hidapi::HidApi::new() {
		Ok(api) => {
			for info in api.device_list() {
				let path = info.path().to_string_lossy().into_owned();
				let e = by_path.entry(path.clone()).or_insert_with(|| HidrawNode {
					path,
					vid: info.vendor_id(),
					pid: info.product_id(),
					name: info.product_string().map(|s| s.to_string()).unwrap_or_default(),
					usages: BTreeSet::new(),
					sysfs: None,
					ro_open: false,
					rw_open: false,
				});
				e.usages.insert((info.usage_page(), info.usage()));
			}
		}
		Err(e) => println!("  hidapi init failed: {e}"),
	}
	for n in by_path.values_mut() {
		if let Some(c) = sysfs_canon(&n.path) {
			n.sysfs = Some(c.to_string_lossy().into_owned());
		}
		n.ro_open = std::fs::File::open(&n.path).is_ok();
		if n.vid == WL_VID {
			n.rw_open = std::fs::OpenOptions::new().read(true).write(true).open(&n.path).is_ok();
		}
	}
	let mut rows: Vec<HidrawNode> = by_path.into_values().collect();
	rows.sort_by(|a, b| a.path.cmp(&b.path));
	for n in &rows {
		let is_vendor = n.vid == WL_VID && n.usages.contains(&(0xFFFFu16, 0u16));
		let usages: Vec<String> = n.usages.iter().map(|(p, u)| format!("0x{p:04x}/0x{u:02x}")).collect();
		let opens = if n.vid == WL_VID {
			format!("RO open: {}   RW open: {}", yn(n.ro_open), yn(n.rw_open))
		} else {
			format!("RO open: {}", yn(n.ro_open))
		};
		let mark = if is_vendor { "   << VENDOR COMMAND IFACE" } else { "" };
		println!(
			"  {}  {:04x}:{:04x}  \"{}\"  usages: {}{}",
			n.path,
			n.vid,
			n.pid,
			n.name,
			usages.join(" "),
			mark
		);
		if let Some(s) = &n.sysfs {
			println!("           sysfs: {}", shorten(s));
		}
		println!("           {opens}");
	}
	rows
}

// ---------------------------------------------------------------- verdict

fn verdict(env: &Env, input: &[InputNode], dongle_paths: &BTreeSet<String>, hidraw: &[HidrawNode], button: u16) {
	println!("\n== verdict for: wl-sniper --button {button} --sniper-stage <A> --normal-stage <B> ==");
	let vendor: Vec<&HidrawNode> = hidraw
		.iter()
		.filter(|h| h.vid == WL_VID && h.usages.contains(&(0xFFFFu16, 0u16)))
		.collect();
	let vendor_ok = match vendor.len() {
		0 => {
			println!("  ✗ no 36A7 vendor command interface (usage_page 0xffff/usage 0) — is the dongle connected? (lsusb | grep -i 36a7)");
			false
		}
		1 => {
			let v = vendor[0];
			if v.rw_open {
				println!("  ✓ vendor command iface: {} (read+write openable)", v.path);
				true
			} else {
				println!(
					"  ✗ vendor iface {} found but NOT read+write openable — udev rule / input-group issue",
					v.path
				);
				false
			}
		}
		n => {
			let list: Vec<&str> = vendor.iter().map(|v| v.path.as_str()).collect();
			println!(
				"  ✗ {n} vendor ifaces ({}) — multiple dongles? wl-sniper requires exactly one",
				list.join(", ")
			);
			false
		}
	};

	let vid_marker = format!("{WL_VID:04x}");
	let dongle_nodes: Vec<&InputNode> = input
		.iter()
		.filter(|i| i.vidpid.as_deref().is_some_and(|m| m.starts_with(&vid_marker)))
		.collect();
	let open: Vec<&InputNode> = dongle_nodes.iter().copied().filter(|i| i.open_err.is_none()).collect();
	let adv: Vec<&InputNode> = open.iter().copied().filter(|i| i.keys.contains(&button)).collect();

	if dongle_nodes.is_empty() {
		println!("  ✗ no dongle evdev nodes found in sysfs at all — dongle disconnected, or kernel hid-parsing issue");
	} else {
		let names: Vec<&str> = dongle_paths.iter().map(|s| s.as_str()).collect();
		println!(
			"  dongle evdev nodes: {} ({} openable by you): {}",
			dongle_nodes.len(),
			open.len(),
			names.join(", ")
		);
		if open.is_empty() && env.euid != 0 {
			println!(
				"  ✗ none openable → 'sudo usermod -aG input {}' then a FULL re-login (or test once with sudo)",
				env.user
			);
		}
		match adv.len() {
			0 => {
				println!(
					"  ✗ no openable dongle node advertises key {} ({}) — in the gm.wlmouse.gg web UI, is the mouse button bound to {} in KEY mode?",
					button,
					key_name(button),
					key_name(button)
				);
				println!("    keys advertised by the dongle's openable nodes:");
				for i in &open {
					let keys = if i.keys.is_empty() {
						"(none)".to_string()
					} else {
						i.keys
							.iter()
							.map(|c| format!("{}({c})", key_name(*c)))
							.collect::<Vec<_>>()
							.join(" ")
					};
					println!("      {}: {keys}", i.path.strip_prefix("/dev/input/").unwrap_or(&i.path));
				}
				println!("    → run again with the right key: wl-probe <KEY>");
			}
			1 => {
				if vendor_ok {
					println!(
						"  ✓ wl-sniper should start: it will grab {} and switch stages over {}.",
						adv[0].path, vendor[0].path
					);
				} else {
					println!(
						"  ~ button node {} is fine, but the vendor iface problem above must be fixed first",
						adv[0].path
					);
				}
			}
			n => {
				let list: Vec<&str> = adv.iter().map(|i| i.path.as_str()).collect();
				println!(
					"  ✗ {n} dongle nodes advertise key {button} ({}) — ambiguous, wl-sniper will refuse: {}",
					list.join(", "),
					key_name(button)
				);
			}
		}
	}
}

// ---------------------------------------------------------------- helpers

fn yn(b: bool) -> &'static str {
	if b { "ok" } else { "NO" }
}

fn shorten(p: &str) -> String {
	if p.len() <= 88 {
		p.to_string()
	} else {
		format!("…/{}", &p[p.len() - 80..])
	}
}

fn ev_name(t: u16) -> &'static str {
	match t {
		0 => "SYN",
		1 => "KEY",
		2 => "REL",
		3 => "ABS",
		4 => "MSC",
		5 => "SW",
		0x0e => "REP",
		0x11 => "LED",
		0x14 => "SND",
		0x15 => "FF",
		0x16 => "PWR",
		_ => "??",
	}
}

fn rel_name(c: u16) -> String {
	match c {
		0 => "X".into(),
		1 => "Y".into(),
		2 => "Z".into(),
		6 => "HWHEEL".into(),
		8 => "WHEEL".into(),
		_ => format!("REL{c}"),
	}
}

fn abs_name(c: u16) -> String {
	match c {
		0 => "X".into(),
		1 => "Y".into(),
		2 => "Z".into(),
		11 => "HAT0X".into(),
		12 => "HAT0Y".into(),
		54 => "VOLUME".into(),
		_ => format!("ABS{c}"),
	}
}

/// `SCROLLLOCK`, `f12`, `BTN_EXTRA`, `70`, `0x46` → evdev code.
fn parse_button(spec: &str) -> Result<u16, String> {
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
		let n: u16 = n.parse().map_err(|_| format!("bad F-key name: {spec}"))?;
		// Linux KEY_F* is NOT contiguous: F1..F10 = 59..=68, F11/F12 = 87/88,
		// F13..F24 = 183..=194.
		return Ok(match n {
			1..=10 => 59 + n - 1,
			11 => 87,
			12 => 88,
			13..=24 => 183 + n - 13,
			_ => return Err(format!("unsupported F-key: {spec} (F1-F24)")),
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
	Err(format!(
		"unrecognized button {spec:?} — use an evdev name (SCROLLLOCK, F12, BTN_EXTRA, ...), a decimal code (70) or hex (0x46)"
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hid_marker_real_paths() {
		assert_eq!(
			hid_marker("/sys/devices/pci0000:00/0000:00:08.1/0000:74:00.4/usb5/5-2/5-2:1.2/0003:36A7:A863.0004/hidraw/hidraw3"),
			Some("36a7:a863".to_string())
		);
		assert_eq!(
			hid_marker("/sys/devices/pci0000:00/0000:00:08.1/0000:74:00.4/usb5/5-2/5-2:1.1/0003:36A7:A863.0002/input/input5/event6"),
			Some("36a7:a863".to_string())
		);
		assert_eq!(hid_marker("/sys/devices/platform/PNP0C0C:00/input/input0/event0"), None);
	}

	#[test]
	fn parse_button_basics() {
		assert_eq!(parse_button("70").unwrap(), 70);
		assert_eq!(parse_button("0x46").unwrap(), 70);
		assert_eq!(parse_button("SCROLLLOCK").unwrap(), 70);
		assert_eq!(parse_button("f12").unwrap(), 88);
		assert!(parse_button("nope").is_err());
	}
}

fn key_name(code: u16) -> String {
	// Linux KEY_F* is NOT contiguous: F1..F10 = 59..=68, F11/F12 = 87/88,
	// F13..F24 = 183..=194.
	if (59..=68).contains(&code) {
		return format!("F{}", code - 58);
	}
	if (87..=88).contains(&code) {
		return format!("F{}", code - 76);
	}
	if (183..=194).contains(&code) {
		return format!("F{}", code - 182);
	}
	match code {
		1 => "ESC",
		19 => "PAUSE",
		57 => "SPACE",
		58 => "CAPSLOCK",
		69 => "NUMLOCK",
		70 => "SCROLLLOCK",
		0x110 => "BTN_LEFT",
		0x111 => "BTN_RIGHT",
		0x112 => "BTN_MIDDLE",
		0x113 => "BTN_SIDE",
		0x114 => "BTN_EXTRA",
		0x115 => "BTN_FORWARD",
		0x116 => "BTN_BACK",
		0x117 => "BTN_TASK",
		_ => return code.to_string(),
	}
	.to_string()
}
