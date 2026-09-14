// HID protocol code derived from wl-mouse (AGPL-3.0-or-later),
// https://heliopolis.live/creations/wl-mouse — trimmed to what wl-sniper
// needs: transport (detect_hid_index, send_and_recv, send_only), profile id,
// and the active-DPI-stage setter. Byte layouts are asserted by the unit
// tests below — keep them in lockstep.

use anyhow::{Result, bail};

pub const WL_VID: u16 = 0x36A7;

pub const REPORT_ID: u8 = 0;
pub const REPORT_SIZE: usize = 64;
pub const RESPONSE_OK: u8 = 0xA1;
pub const RESPONSE_SLEEPING: u8 = 0xA0;
pub const MAX_RETRIES: u8 = 10;

pub const KNOWN_PIDS: &[(u16, &str)] = &[
	(0xA864, "HUAN (wired)"),
	(0xA863, "HUAN (dongle)"),
	(0xA867, "BEAST MIAO (wired)"),
	(0xA866, "BEAST MIAO (dongle)"),
	(0xA882, "WLmouse (1K dongle)"),
	(0xA873, "STRIDER (wired)"),
	(0xA872, "STRIDER (dongle)"),
	(0xA875, "YING (wired)"),
	(0xA874, "YING (dongle)"),
	(0xA879, "SWORD X (wired)"),
	(0xA878, "SWORD X (dongle)"),
	(0xA886, "BEAST MINI (wired)"),
	(0xA885, "BEAST MINI (dongle)"),
	(0xA869, "BEAST MINI PRO (wired)"),
	(0xA868, "BEAST MINI PRO (dongle)"),
	(0xA881, "BEAST MAX (wired)"),
	(0xA880, "BEAST MAX (dongle)"),
	(0xA884, "BEAST X (wired)"),
	(0xA883, "BEAST X (dongle)"),
	(0xA871, "BEAST X PRO (wired)"),
	(0xA870, "BEAST X PRO (dongle)"),
];

pub struct HidTransport<'a> {
	device: &'a hidapi::HidDevice,
	pub hid_index: u8,
}

impl<'a> HidTransport<'a> {
	pub fn new(device: &'a hidapi::HidDevice) -> Self {
		Self { device, hid_index: 0 }
	}

	/// Slow path: write a feature report, sleep 30 ms, read the response.
	/// Retries while the mouse answers "sleeping". Startup-only by design —
	/// the button path uses `send_only`.
	pub fn send_and_recv(&self, data: &[u8; REPORT_SIZE]) -> Result<[u8; REPORT_SIZE]> {
		let send_buf: Vec<u8> = std::iter::once(REPORT_ID).chain(data.iter().copied()).collect();
		let cmd_byte = data[5];

		for attempt in 0..MAX_RETRIES {
			self.device.send_feature_report(&send_buf).map_err(anyhow::Error::msg)?;

			std::thread::sleep(std::time::Duration::from_millis(30));

			let mut buf = [0u8; REPORT_SIZE + 1];
			buf[0] = REPORT_ID;
			self.device.get_feature_report(&mut buf).map_err(anyhow::Error::msg)?;

			let resp = &buf[1..];
			let check_idx = 1 - self.hid_index as usize;

			if resp[check_idx] == RESPONSE_OK && resp[5] == cmd_byte {
				let mut result = [0u8; REPORT_SIZE];
				result.copy_from_slice(resp);
				return Ok(result);
			}

			if resp[check_idx] == RESPONSE_SLEEPING || (resp[check_idx] == RESPONSE_OK && resp[5] != cmd_byte) {
				if attempt == 0 {
					eprintln!("    mouse is asleep, waiting for it to wake up (wiggle it if needed)...");
				}
				std::thread::sleep(std::time::Duration::from_millis(500));
				continue;
			}

			if attempt < MAX_RETRIES - 1 {
				std::thread::sleep(std::time::Duration::from_millis(50));
			}
		}
		bail!("no response from mouse (is it asleep or out of range?)")
	}

	/// Fast path: write a feature report, do not read anything. This is what
	/// the button hold/release edges use — fire-and-forget, stateless and
	/// idempotent (the next edge re-sends a stage set, so a dropped write
	/// self-heals).
	pub fn send_only(&self, data: &[u8; REPORT_SIZE]) -> Result<()> {
		let send_buf: Vec<u8> = std::iter::once(REPORT_ID).chain(data.iter().copied()).collect();
		self.device.send_feature_report(&send_buf).map_err(anyhow::Error::msg)?;
		Ok(())
	}

	/// Wired vs dongle firmware replies at different offsets; detect once.
	pub fn detect_hid_index(&mut self) -> Result<()> {
		let cmd = build_get_firmware(0x02);
		let send_buf: Vec<u8> = std::iter::once(REPORT_ID).chain(cmd.iter().copied()).collect();

		for attempt in 0..MAX_RETRIES {
			self.device.send_feature_report(&send_buf).map_err(anyhow::Error::msg)?;
			std::thread::sleep(std::time::Duration::from_millis(30));

			let mut buf = [0u8; REPORT_SIZE + 1];
			buf[0] = REPORT_ID;
			self.device.get_feature_report(&mut buf).map_err(anyhow::Error::msg)?;

			let resp = &buf[1..];
			if resp[0] == RESPONSE_OK {
				self.hid_index = 1;
				return Ok(());
			} else if resp[1] == RESPONSE_OK {
				self.hid_index = 0;
				return Ok(());
			}

			if resp[0] == RESPONSE_SLEEPING || resp[1] == RESPONSE_SLEEPING {
				if attempt == 0 {
					eprintln!("    mouse is asleep, waiting for it to wake up (wiggle it if needed)...");
				}
				std::thread::sleep(std::time::Duration::from_millis(500));
			}
		}
		bail!("no response from mouse (is it asleep or out of range?)")
	}
}

fn build_profile_get(len: u8, page: u8, cmd: u8, profile: u8) -> [u8; REPORT_SIZE] {
	let mut buf = [0u8; REPORT_SIZE];
	buf[2] = 0x02;
	buf[3] = len;
	buf[4] = page;
	buf[5] = cmd;
	buf[6] = profile;
	buf
}

fn build_profile_set(len: u8, page: u8, cmd: u8, profile: u8, value: u8) -> [u8; REPORT_SIZE] {
	let mut buf = build_profile_get(len, page, cmd, profile);
	buf[7] = value;
	buf
}

// Kept for the byte-layout tests and protocol completeness; the daemon's
// only response field (active profile) sits at [7 - hid_index], not here.
#[allow(dead_code)]
pub fn resp_u8(resp: &[u8; REPORT_SIZE], hid_index: u8) -> u8 {
	resp[(8 - hid_index) as usize]
}

pub fn build_get_firmware(device_id: u8) -> [u8; REPORT_SIZE] {
	let mut buf = [0u8; REPORT_SIZE];
	buf[2] = device_id;
	buf[3] = 0x10;
	buf[4] = 0x00;
	buf[5] = 0x81;
	buf
}

pub fn build_get_profile_id() -> [u8; REPORT_SIZE] {
	let mut buf = [0u8; REPORT_SIZE];
	buf[2] = 0x02;
	buf[3] = 0x01;
	buf[4] = 0x00;
	buf[5] = 0x85;
	buf
}

/// WRITE — set active DPI stage. `stage` is 1-based (1..=6) and is sent
/// verbatim. Fire-and-forget on the button path.
pub fn build_set_active_dpi(profile: u8, stage: u8) -> [u8; REPORT_SIZE] {
	build_profile_set(0x02, 0x01, 0x02, profile, stage)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn set_active_dpi_report_bytes() {
		let b = build_set_active_dpi(1, 2);
		assert_eq!(&b[2..8], &[0x02, 0x02, 0x01, 0x02, 0x01, 0x02]);
		assert!(b[8..].iter().all(|&x| x == 0));
	}

	#[test]
	fn get_profile_id_report_bytes() {
		let b = build_get_profile_id();
		assert_eq!(&b[2..6], &[0x02, 0x01, 0x00, 0x85]);
		assert!(b[6..].iter().all(|&x| x == 0));
	}

	#[test]
	fn active_dpi_response_is_0_based() {
		// The active-stage *readout* is 0-based; the *set* command above is
		// 1-based. (wl-sniper never reads the active stage for defaults —
		// the readout was observed unreliable — but keep the invariant.)
		let hid_index = 0u8;
		let mut resp = [0u8; REPORT_SIZE];
		resp[(8 - hid_index) as usize] = 1; // stage index 1 => stage 2 (1-based)
		assert_eq!(resp_u8(&resp, hid_index) + 1, 2);

		let mut resp = [0u8; REPORT_SIZE];
		resp[(8 - 1) as usize] = 0; // stage index 0 => stage 1 (1-based), hid_index 1
		assert_eq!(resp_u8(&resp, 1) + 1, 1);
	}
}
