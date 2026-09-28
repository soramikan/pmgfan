//! Fujitsu OEM IPMI コマンドのペイロード構築。
//! makeding/fujitsu-1320-m4-fancontrol（irmc_fan.py）互換。
//!
//! - IANA 0x002880 → little endian `80 28 00`
//! - NetFn 0x2e / Command 0xf5
//! - `W`(0x57): PWM 強制/解除、 `R`(0x52): force スロット読み出し

use crate::backend::{IpmiError, PwmSlot, Result};

pub const IANA_LE: [u8; 3] = [0x80, 0x28, 0x00];
pub const NETFN: u8 = 0x2e;
pub const COMMAND: u8 = 0xf5;

/// デフォルトの安全下限。これ未満の PWM は明示的に許可しない限り拒否する。
pub const MIN_SAFE_PWM: u8 = 30;
pub const MAX_PWM: u8 = 100;

/// 全チャンネル指定のスコープ値。
pub const SCOPE_ALL: u8 = 0xff;
/// シャーシファン（FAN CPU / FANx SYS）のみ指定のスコープ値。
/// 実機観測: PSU スロットはこのスコープに含まれず、iRMC の
/// 自動制御に残る。このファームウェアが `W` で受理するのは
/// 0x03 と 0xff のみ。
pub const SCOPE_CHASSIS: u8 = 0x03;
/// 「強制」フラグ。
const FLAG_FORCE: u8 = 0x80;

/// スロット読み出しの既定 index（irmc_fan.py の read 既定値）。
pub const DEFAULT_SLOT_INDICES: [u8; 4] = [0x00, 0x01, 0x19, 0x1a];

fn signature(tag: u8) -> [u8; 4] {
    // "-F" + tag + 0x01（例: "-FW\x01"）
    [0x2d, 0x46, tag, 0x01]
}

/// `raw` コマンド全体のデータ部（NetFn/Cmd を除く）。
/// `80 28 00 | 2d 46 57 01 | <scope> 80 <pwm>`
/// `scope` は `SCOPE_ALL`（全ファン）または `SCOPE_CHASSIS`
/// （シャーシファンのみ。PSU は iRMC 自動制御に残る）。
pub fn set_pwm_data(scope: u8, pwm: u8) -> Vec<u8> {
    debug_assert!(pwm <= MAX_PWM);
    debug_assert!(scope == SCOPE_ALL || scope == SCOPE_CHASSIS);
    let mut v = Vec::with_capacity(10);
    v.extend_from_slice(&IANA_LE);
    v.extend_from_slice(&signature(b'W'));
    v.extend_from_slice(&[scope, FLAG_FORCE, pwm]);
    v
}

/// PWM 強制解除のデータ部。
/// `80 28 00 | 2d 46 57 01 | ff 00 00`
/// 解除は常に全スコープ（シャーシ強制の解除にもこれを使う）。
pub fn clear_override_data() -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    v.extend_from_slice(&IANA_LE);
    v.extend_from_slice(&signature(b'W'));
    v.extend_from_slice(&[SCOPE_ALL, 0x00, 0x00]);
    v
}

/// force スロット読み出しのデータ部。
/// `80 28 00 | 2d 46 52 01 | <count> | [idx 00]...`
pub fn read_slots_data(indices: &[u8]) -> Result<Vec<u8>> {
    if indices.is_empty() || indices.len() > 31 {
        return Err(IpmiError::Parse(format!(
            "slot index count must be 1..31, got {}",
            indices.len()
        )));
    }
    let mut v = Vec::with_capacity(8 + indices.len() * 2);
    v.extend_from_slice(&IANA_LE);
    v.extend_from_slice(&signature(b'R'));
    v.push(indices.len() as u8);
    for &idx in indices {
        if idx > 31 {
            return Err(IpmiError::Parse(format!(
                "slot index out of range 0..31: {idx}"
            )));
        }
        v.extend_from_slice(&[idx, 0x00]);
    }
    Ok(v)
}

/// `ipmitool raw` の応答テキスト（`" 80 28 00 ..."`）をバイト列にする。
pub fn parse_hex_bytes(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .filter_map(|t| u8::from_str_radix(t, 16).ok())
        .collect()
}

/// read_slots の応答をデコードする。
///
/// 実機観測フォーマット: `IANA(3B) | 0x01 | count | (index|flags, value)×count`
/// - 例: `80 28 00 01 04 40 59 01 00 19 00 1a 00`（index 0,1,0x19,0x1a の読み出し）
/// - index バイトの下位6bitが index、bit7=強制フラグ（強制40%時 `c0 28`）
///
/// フォーマットに合わない応答は空を返す。
pub fn decode_slots(resp: &[u8], _requested: &[u8]) -> Vec<PwmSlot> {
    let body = if resp.len() >= 3 && resp[..3] == IANA_LE {
        &resp[3..]
    } else {
        resp
    };
    // [0x01, count] ヘッダ + count ペア
    if body.len() < 2 || body[0] != 0x01 {
        return Vec::new();
    }
    let count = body[1] as usize;
    let pairs = &body[2..];
    if pairs.len() < count * 2 {
        return Vec::new();
    }
    pairs[..count * 2]
        .chunks_exact(2)
        .map(|c| PwmSlot {
            index: c[0] & 0x3f,
            forced: c[0] & 0x80 != 0,
            value: c[1],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_pwm_40_matches_reference_payload() {
        assert_eq!(
            set_pwm_data(SCOPE_ALL, 40),
            vec![0x80, 0x28, 0x00, 0x2d, 0x46, 0x57, 0x01, 0xff, 0x80, 0x28]
        );
        // chassis スコープは先頭バイトだけが変わる
        assert_eq!(
            set_pwm_data(SCOPE_CHASSIS, 40),
            vec![0x80, 0x28, 0x00, 0x2d, 0x46, 0x57, 0x01, 0x03, 0x80, 0x28]
        );
    }

    #[test]
    fn clear_matches_reference_payload() {
        assert_eq!(
            clear_override_data(),
            vec![0x80, 0x28, 0x00, 0x2d, 0x46, 0x57, 0x01, 0xff, 0x00, 0x00]
        );
    }

    #[test]
    fn read_slots_matches_reference_payload() {
        assert_eq!(
            read_slots_data(&[0x00, 0x01, 0x19, 0x1a]).unwrap(),
            vec![
                0x80, 0x28, 0x00, 0x2d, 0x46, 0x52, 0x01, 0x04, 0x00, 0x00, 0x01, 0x00, 0x19, 0x00,
                0x1a, 0x00
            ]
        );
    }

    #[test]
    fn read_slots_validates_input() {
        assert!(read_slots_data(&[]).is_err());
        assert!(read_slots_data(&[0x20]).is_err());
        assert!(read_slots_data(&[0u8; 32]).is_err());
    }

    #[test]
    fn parse_hex_bytes_from_raw_output() {
        assert_eq!(
            parse_hex_bytes(" 80 28 00 0a ff\n"),
            vec![0x80, 0x28, 0x00, 0x0a, 0xff]
        );
        assert!(parse_hex_bytes("").is_empty());
        assert_eq!(parse_hex_bytes(" zz 11"), vec![0x11]);
    }

    #[test]
    fn decode_slots_real_response_auto() {
        // index 0,1,0x19,0x1a の自動制御時の実機応答
        let resp = [
            0x80, 0x28, 0x00, 0x01, 0x04, 0x40, 0x59, 0x01, 0x00, 0x19, 0x00, 0x1a, 0x00,
        ];
        let slots = decode_slots(&resp, &[0, 1, 0x19, 0x1a]);
        assert_eq!(
            slots,
            vec![
                PwmSlot {
                    index: 0x00,
                    forced: false,
                    value: 0x59
                },
                PwmSlot {
                    index: 0x01,
                    forced: false,
                    value: 0x00
                },
                PwmSlot {
                    index: 0x19,
                    forced: false,
                    value: 0x00
                },
                PwmSlot {
                    index: 0x1a,
                    forced: false,
                    value: 0x00
                },
            ]
        );
    }

    #[test]
    fn decode_slots_real_response_forced_40() {
        // 40% 強制時の実機応答（index0 → 0xc0 = index0|bit7, value 0x28）
        let resp = [
            0x80, 0x28, 0x00, 0x01, 0x04, 0xc0, 0x28, 0x01, 0x00, 0x19, 0x00, 0x1a, 0x00,
        ];
        let slots = decode_slots(&resp, &[0, 1, 0x19, 0x1a]);
        assert_eq!(slots[0].index, 0x00);
        assert!(slots[0].forced);
        assert_eq!(slots[0].value, 40);
    }
}
