//! `/dev/ipmi0` 直接アクセスのネイティブバックエンド（Phase 9）。
//!
//! ipmitool プロセスを介さず、OpenIPMI ドライバの ioctl
//! （`IPMICTL_SEND_COMMAND` + `poll` + `IPMICTL_RECEIVE_MSG`）で
//! iRMC と直接やり取りする。
//!
//! - SDR: `Reserve SDR Repository` → `Get SDR` でレコード列挙し、
//!   Full Sensor Record (0x01) をパース
//! - 読み取り: `Get Sensor Reading` の raw 値をレコードの
//!   M/B/R_exp/B_exp で線形化（`y = (Mx + B·10^Bexp)·10^Rexp`）
//! - OEM 制御: NetFn 0x2e / Cmd 0xf5 をそのまま送信
//! - `model_name()`: FRU を順に走査し、最初に見つかった Product Info
//!   Area の Product Name を返す（TX1320 M4 では FRU 2 = Chassis）

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmgfan_core::control::PwmScope;
use pmgfan_core::fan::{FanReading, FanStatus};
use pmgfan_core::sensor::TempReading;

use crate::backend::{FanControlBackend, IpmiError, Result};
use crate::fujitsu;

/// 1回の IPMI 要求の応答待ち上限。ローカル KCS は通常数 ms で応答する。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
/// `Get SDR`/`Read FRU Data` の1リクエストあたりの最大要求バイト数。
const SDR_READ_CHUNK: usize = 64;
/// SDR スキャン中に予約が無効化された場合の再試行上限。
const SCAN_RETRIES: u32 = 3;
/// SDR レコード数の上限。異常な BMC が `next_id` で長い/循環する
/// チェーンを返しても、デバイスロックを握ったまま無限巡回しないための上限。
/// （実機 TX1320 M4 は ~156 レコード）
const MAX_SDR_RECORDS: usize = 1024;
/// `O_NONBLOCK` 下で SMI が忙しいと SEND_COMMAND が EAGAIN を返す。
/// ポーリング毎に失敗としてカウントしないよう短いバックオフで再試行する。
const SEND_RETRIES: u32 = 5;
const SEND_RETRY_DELAY: Duration = Duration::from_millis(20);
/// IPMI メッセージのカーネル上限（`IPMI_MAX_MSG_LENGTH`）。
const IPMI_MAX_MSG_LENGTH: usize = 272;
/// FRU デバイス探索の上限 ID。
const MAX_FRU_DEVICES: u8 = 8;
/// パース済み SDR センサー一覧のキャッシュ寿命。BMC FW 更新や
/// PSU 増設などでセンサー集合が変わった場合の自動追従用。
const SENSOR_CACHE_TTL: Duration = Duration::from_secs(300);

const NETFN_SENSOR: u8 = 0x04;
const NETFN_APP: u8 = 0x06;
const NETFN_STORAGE: u8 = 0x0a;

const CMD_GET_DEVICE_ID: u8 = 0x01;
const CMD_GET_FRU_AREA_INFO: u8 = 0x10;
const CMD_READ_FRU_DATA: u8 = 0x11;
const CMD_RESERVE_SDR: u8 = 0x22;
const CMD_GET_SDR: u8 = 0x23;
const CMD_GET_SENSOR_READING: u8 = 0x2d;

const REC_TYPE_FULL_SENSOR: u8 = 0x01;
const SENSOR_TYPE_TEMPERATURE: u8 = 0x01;
const SENSOR_TYPE_FAN: u8 = 0x04;
const UNIT_DEGREES_C: u8 = 0x01;

// ---- Linux OpenIPMI ioctl ABI (linux/ipmi.h) ----

#[repr(C)]
struct IpmiMsg {
    netfn: u8,
    cmd: u8,
    data_len: u16,
    data: *mut u8,
}

#[repr(C)]
struct IpmiReq {
    addr: *mut u8,
    addr_len: u32,
    msgid: i64,
    msg: IpmiMsg,
}

#[repr(C)]
struct IpmiRecv {
    recv_type: i32,
    addr: *mut u8,
    addr_len: u32,
    msgid: i64,
    msg: IpmiMsg,
}

#[repr(C)]
struct IpmiSystemInterfaceAddr {
    addr_type: i32,
    channel: i16,
    lun: u8,
}

const IPMI_SYSTEM_INTERFACE_ADDR_TYPE: i32 = 0x0c;
const IPMI_BMC_CHANNEL: i16 = 0xf;
const IPMI_RESPONSE_RECV_TYPE: i32 = 1;

const fn ioc(dir: u64, ty: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (ty << 8) | nr | (size << 16)
}
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const IPMI_IOC_MAGIC: u64 = b'i' as u64;
const IPMICTL_SEND_COMMAND: u64 = ioc(
    IOC_READ,
    IPMI_IOC_MAGIC,
    13,
    std::mem::size_of::<IpmiReq>() as u64,
);
const IPMICTL_RECEIVE_MSG: u64 = ioc(
    IOC_READ | IOC_WRITE,
    IPMI_IOC_MAGIC,
    12,
    std::mem::size_of::<IpmiRecv>() as u64,
);
/// バッファに収まらないメッセージを切り捨てて受け取る版。
/// IPMICTL_RECEIVE_MSG は EMSGSIZE を返しつつメッセージを受信キューに
/// 残すため、回復用に使用する。
const IPMICTL_RECEIVE_MSG_TRUNC: u64 = ioc(
    IOC_READ | IOC_WRITE,
    IPMI_IOC_MAGIC,
    11,
    std::mem::size_of::<IpmiRecv>() as u64,
);

// 手書き ABI のサイズを固定する（カーネル uapi と一致しない場合は
// コンパイル時に検出する）
const _: () = assert!(std::mem::size_of::<IpmiMsg>() == 16);
const _: () = assert!(std::mem::size_of::<IpmiReq>() == 40);
const _: () = assert!(std::mem::size_of::<IpmiRecv>() == 48);
const _: () = assert!(std::mem::size_of::<IpmiSystemInterfaceAddr>() == 8);

// ---- 低レベル送受信 ----

/// シリアライズされた IPMI デバイス。send→poll→recv をアトミックに行う。
struct Device {
    file: File,
    msgid: i64,
}

/// IPMI 要求を1回送受信する最小トランスポート。
/// `Device`（/dev/ipmi0 ioctl）とテスト用モックが実装する。
trait Transport {
    /// completion code 剥がし済みの応答データを返す。
    /// cc != 0 は `IpmiError::Completion`。
    fn request(&mut self, netfn: u8, cmd: u8, data: &[u8]) -> Result<Vec<u8>>;
}

impl Transport for Device {
    fn request(&mut self, netfn: u8, cmd: u8, data: &[u8]) -> Result<Vec<u8>> {
        Device::request(self, netfn, cmd, data)
    }
}

impl Device {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            // 受信キューが空のとき RECEIVE_MSG がブロックせず
            // EAGAIN で返るようにする（poll で受信をゲートしている前提）
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(IpmiError::Io)?;
        Ok(Self { file, msgid: 0 })
    }

    /// netfn/cmd/data で1要求を送り、completion code を剥がした
    /// 応答データを返す。cc != 0 は `IpmiError::Completion`。
    fn request(&mut self, netfn: u8, cmd: u8, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() > IPMI_MAX_MSG_LENGTH {
            // カーネル側のメッセージ長上限。超過は静音切り詰めにせず拒否する
            return Err(IpmiError::Parse("ipmi request payload too large".into()));
        }
        self.msgid = self.msgid.wrapping_add(1);
        let msgid = self.msgid;

        let saddr = IpmiSystemInterfaceAddr {
            addr_type: IPMI_SYSTEM_INTERFACE_ADDR_TYPE,
            channel: IPMI_BMC_CHANNEL,
            lun: 0,
        };
        let req = IpmiReq {
            addr: &saddr as *const _ as *mut u8,
            addr_len: std::mem::size_of::<IpmiSystemInterfaceAddr>() as u32,
            msgid,
            msg: IpmiMsg {
                netfn,
                cmd,
                data_len: data.len() as u16,
                data: data.as_ptr() as *mut u8,
            },
        };
        // O_NONBLOCK 下では BMC/SMI が忙しいと EAGAIN で失敗する。
        // 一過性の輻輳で poll 失敗扱いにならないよう短く再試行する。
        let mut send_attempts = 0u32;
        loop {
            let r = unsafe { libc::ioctl(self.file.as_raw_fd(), IPMICTL_SEND_COMMAND, &req) };
            if r >= 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if e.raw_os_error() == Some(libc::EAGAIN) && send_attempts < SEND_RETRIES {
                send_attempts += 1;
                std::thread::sleep(SEND_RETRY_DELAY);
                continue;
            }
            return Err(e.into());
        }

        // 応答を poll → receive。非同期イベントや msgid の不一致は捨てる。
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut data_buf = [0u8; 512];
        let mut addr_buf = [0u8; 40]; // sizeof(struct ipmi_addr)
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(IpmiError::Timeout(REQUEST_TIMEOUT.as_secs()));
            }
            let mut pfd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if n == 0 {
                // 期限切れ間際の poll タイムアウト。次の周回で
                // deadline チェックが抜ける
                continue;
            }
            if pfd.revents & libc::POLLIN == 0 {
                // POLLERR / POLLHUP / POLLNVAL 等。デバイス異常として
                // 即エラーにする（busy-spin しない）
                return Err(std::io::Error::other(format!(
                    "ipmi device poll error (revents {:#x})",
                    pfd.revents
                ))
                .into());
            }

            // 入力側の data_len/addr_len はバッファ容量。
            // 0 のまま渡すと EMSGSIZE になる（実機観測）。
            let mut recv = IpmiRecv {
                recv_type: 0,
                addr: addr_buf.as_mut_ptr(),
                addr_len: addr_buf.len() as u32,
                msgid: 0,
                msg: IpmiMsg {
                    netfn: 0,
                    cmd: 0,
                    data_len: data_buf.len() as u16,
                    data: data_buf.as_mut_ptr(),
                },
            };
            let r = unsafe { libc::ioctl(self.file.as_raw_fd(), IPMICTL_RECEIVE_MSG, &mut recv) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // キューが空（前のポーリングで通知済みのメッセージを
                    // 別メッセージが先に使い切った等）
                    Some(libc::EAGAIN) => {
                        // POLLIN と共に立った POLLERR/HUP（データ枯渇後の
                        // 切断）では再 poll が同じ revents を返し続けるので、
                        // デッドラインまで回さず即エラーにする
                        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                            return Err(std::io::Error::other(format!(
                                "ipmi device error after poll (revents {:#x})",
                                pfd.revents
                            ))
                            .into());
                        }
                        continue;
                    }
                    // 受信バッファに収まらないメッセージはキューに残り、
                    // 以降の受信が EMSGSIZE を返し続けるので
                    // TRUNC 版で読み捨てて回復する。
                    // （data_buf=512B > IPMI_MAX_MSG_LENGTH(272B) のため
                    //   通常は到達しない）
                    Some(libc::EMSGSIZE) => {
                        let mut drop_recv = IpmiRecv {
                            recv_type: 0,
                            addr: addr_buf.as_mut_ptr(),
                            addr_len: addr_buf.len() as u32,
                            msgid: 0,
                            msg: IpmiMsg {
                                netfn: 0,
                                cmd: 0,
                                data_len: data_buf.len() as u16,
                                data: data_buf.as_mut_ptr(),
                            },
                        };
                        unsafe {
                            libc::ioctl(
                                self.file.as_raw_fd(),
                                IPMICTL_RECEIVE_MSG_TRUNC,
                                &mut drop_recv,
                            )
                        };
                        continue;
                    }
                    _ => return Err(e.into()),
                }
            }
            // 自前の要求に対する応答だけを受け取る（非同期イベント等は捨てる）
            if recv.recv_type != IPMI_RESPONSE_RECV_TYPE
                || recv.msgid != msgid
                || recv.msg.netfn != netfn | 1
                || recv.msg.cmd != cmd
            {
                continue;
            }
            let len = (recv.msg.data_len as usize).min(data_buf.len());
            let data = &data_buf[..len];
            if data.is_empty() {
                return Err(IpmiError::Parse("empty ipmi response".into()));
            }
            if data[0] != 0 {
                return Err(IpmiError::Completion(data[0]));
            }
            return Ok(data[1..].to_vec());
        }
    }
}

// ---- SDR ----

/// Full Sensor Record (type 0x01) のパース結果。
#[derive(Debug, Clone)]
struct FullSensor {
    sensor_num: u8,
    sensor_type: u8,
    event_type: u8,
    /// unit byte1 [7:6]: 0=unsigned 1=1's 2=2's complement 3=no analog
    analog_fmt: u8,
    unit_base: u8,
    linearization: u8,
    m: i32,
    b: i32,
    r_exp: i32,
    b_exp: i32,
    name: String,
}

fn sext10(v: u32) -> i32 {
    if v & 0x200 != 0 {
        (v as i32) - 0x400
    } else {
        v as i32
    }
}

fn sext4(v: u8) -> i32 {
    if v & 0x08 != 0 {
        (v as i32) - 0x10
    } else {
        v as i32
    }
}

/// センサー ID 文字列のデコード。type bits: 3=ASCII, 2=6-bit packed。
/// それ以外（unicode/BCD）は None。
fn decode_id_string(id_code: u8, data: &[u8]) -> Option<String> {
    let len = (id_code & 0x3f) as usize;
    match id_code >> 6 {
        3 => {
            let s: String = data
                .iter()
                .take(len)
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        ' '
                    }
                })
                .collect();
            Some(s.trim_end().to_string())
        }
        2 => {
            // 6-bit ASCII: [5:0] length は packed データの
            // **バイト数**（3バイト→4文字）。末尾の不完全グループは
            // 残バイト数で解ける文字だけ取り出す（1B→1文字, 2B→2文字）。
            let data = &data[..len.min(data.len())];
            let mut out = String::new();
            let mut pos = 0;
            while pos < data.len() {
                let b0 = data[pos] as u32;
                let b1 = *data.get(pos + 1).unwrap_or(&0) as u32;
                let b2 = *data.get(pos + 2).unwrap_or(&0) as u32;
                let mut push = |c: u32| {
                    out.push(char::from_u32((c & 0x3f) + 0x20).unwrap_or(' '));
                };
                push(b0);
                if pos + 1 < data.len() {
                    push((b0 >> 6) | ((b1 & 0x0f) << 2));
                }
                if pos + 2 < data.len() {
                    push((b1 >> 4) | ((b2 & 0x03) << 4));
                    push(b2 >> 2);
                }
                pos += 3;
            }
            Some(out.trim_end().to_string())
        }
        _ => None,
    }
}

fn parse_full_sensor(rec: &[u8]) -> Option<FullSensor> {
    // header(5) + 最低限の本体。id_code(47) + 最大16バイトID文字列。
    if rec.len() < 48 || rec[3] != REC_TYPE_FULL_SENSOR {
        return None;
    }
    let id_code = rec[47];
    let name = decode_id_string(id_code, &rec[48..]).unwrap_or_default();
    Some(FullSensor {
        sensor_num: rec[7],
        sensor_type: rec[12],
        // bit7 は reserved — 下位7bit のみが event/reading type code
        event_type: rec[13] & 0x7f,
        analog_fmt: rec[20] >> 6,
        unit_base: rec[21],
        // bit7 は reserved
        linearization: rec[23] & 0x7f,
        m: sext10(rec[24] as u32 | (((rec[25] & 0xc0) as u32) << 2)),
        b: sext10(rec[26] as u32 | (((rec[27] & 0xc0) as u32) << 2)),
        r_exp: sext4(rec[29] >> 4),
        b_exp: sext4(rec[29] & 0x0f),
        name,
    })
}

/// `y = (M·x + B·10^Bexp) · 10^Rexp`。非線形/非アナログは None。
fn linearize(s: &FullSensor, raw: u8) -> Option<f64> {
    if s.linearization != 0 || s.analog_fmt == 3 {
        return None;
    }
    let x = match s.analog_fmt {
        0 => raw as f64,
        2 => (raw as i8) as f64,
        1 => {
            let v = raw as i32;
            if v & 0x80 != 0 {
                -((!v) & 0x7f) as f64
            } else {
                v as f64
            }
        }
        _ => return None,
    };
    Some((s.m as f64 * x + s.b as f64 * 10f64.powi(s.b_exp)) * 10f64.powi(s.r_exp))
}

/// `Get Sensor Reading` の応答。cc != 0 / reading unavailable は
/// `Err(Completion)` ではなく呼び出し側で扱えるよう分離する。
enum SensorReading {
    /// raw 値としきい値比較ビット（event_type==0x01 のときのみ有効）
    Value {
        raw: u8,
        thr: u8,
    },
    Unavailable,
}

/// `Get Sensor Reading` 応答（completion code 剥がし済み）のパース。
/// 応答 byte2 のフラグ構成（IPMI Table 35-15）:
///   bit7 = event messages enabled（0 = 無効）
///   bit6 = sensor scanning enabled（**0 = 無効**。無効時の生値は
///          最終ラッチ値であり、生きた読み取りではない → 欠測扱い）
///   bit5 = reading/state unavailable
///   bit4:0 = reserved
/// 実機（iRMC S5）の健全センサーは 0xc0 を返す。
fn parse_sensor_reading(d: &[u8]) -> Result<SensorReading> {
    if d.len() < 2 {
        return Err(IpmiError::Parse("short sensor reading".into()));
    }
    if d[1] & 0x20 != 0 || d[1] & 0x40 == 0 {
        return Ok(SensorReading::Unavailable);
    }
    Ok(SensorReading::Value {
        raw: d[0],
        thr: *d.get(2).unwrap_or(&0),
    })
}

fn get_sensor_reading(dev: &mut impl Transport, sensor_num: u8) -> Result<SensorReading> {
    match dev.request(NETFN_SENSOR, CMD_GET_SENSOR_READING, &[sensor_num]) {
        Ok(d) => parse_sensor_reading(&d),
        // 「このセンサーに読み取り値が存在しない」ことを意味する
        // completion code のみ欠測扱いにする:
        //   0xcb = requested sensor/data/record not present
        //   0xcd = command illegal for this sensor/record type
        // それ以外（0xc0 busy, 0xc1 invalid cmd, 0xc3 timeout, 0xff 等の
        // 障害系）は Err 伝播 — 全センサーが Disabled に見えても
        // poll_fans_loop が成功扱いし ipmi_failure_limit が働かない
        // 状態を防ぐ。実機観測: 不在ファンは cc ではなく status byte の
        // unavailable ビット（0xa0）で返る。
        Err(IpmiError::Completion(0xcb | 0xcd)) => Ok(SensorReading::Unavailable),
        Err(e) => Err(e),
    }
}

/// SDR リポジトリを走査して全レコードのバイト列を返す。
/// 予約 ID の無効化（別エージェントの介入）に備えてリトライする。
fn scan_sdr(dev: &mut impl Transport) -> Result<Vec<Vec<u8>>> {
    for _ in 0..SCAN_RETRIES {
        let resv = dev.request(NETFN_STORAGE, CMD_RESERVE_SDR, &[])?;
        if resv.len() < 2 {
            return Err(IpmiError::Parse("short reserve response".into()));
        }
        let resv_id = resv[0] as u16 | (resv[1] as u16) << 8;

        let mut records = Vec::new();
        let mut id: u16 = 0;
        let mut seen = HashSet::new();
        let mut reservation_lost = false;

        while id != 0xffff {
            // BMC が next_id を誤って返した場合の無限ループ防止。
            // レコード ID の再訪（自己ループ・循環）と異常に長い
            // チェーンの両方をここで打ち切る
            if !seen.insert(id) || seen.len() > MAX_SDR_RECORDS {
                return Err(IpmiError::Parse(format!(
                    "sdr record chain broken at id {id:#06x} ({} records)",
                    records.len()
                )));
            }
            // レコードはチャンク読み。ヘッダ5バイトで本体長を確定させる。
            // rec[4] は u8 なので total <= 5+255 = 260 が上限。
            let mut rec = Vec::new();
            let mut next_id = 0xffffu16;
            loop {
                // ヘッダが揃ったら本体長から総サイズを確定し、
                // レコード末尾を超えて要求しないようクランプする
                let total = if rec.len() >= 5 {
                    5 + rec[4] as usize
                } else {
                    5
                };
                let remaining = total.saturating_sub(rec.len());
                if remaining == 0 {
                    break;
                }
                // Get SDR の offset は u8 なので 255 まで。それを超える
                // offset は発行できない（実機では rec[4]<=255 → total<=260
                // であり、offset=255 から残り全量(<=5B)を読めば完了する）。
                // rec.len()<255 では次の offset が 255 を超えないよう
                // 要求量を制限する。
                if rec.len() > 255 {
                    return Err(IpmiError::Parse(format!(
                        "sdr record {id:#06x} exceeds addressable size"
                    )));
                }
                let want = if rec.len() == 255 {
                    remaining.min(SDR_READ_CHUNK)
                } else {
                    remaining.min(SDR_READ_CHUNK).min(255 - rec.len())
                } as u8;
                let req = [
                    resv_id as u8,
                    (resv_id >> 8) as u8,
                    id as u8,
                    (id >> 8) as u8,
                    rec.len() as u8,
                    want,
                ];
                match dev.request(NETFN_STORAGE, CMD_GET_SDR, &req) {
                    Ok(d) => {
                        if d.len() < 2 {
                            return Err(IpmiError::Parse("short sdr response".into()));
                        }
                        next_id = d[0] as u16 | (d[1] as u16) << 8;
                        let got = &d[2..];
                        if got.is_empty() {
                            return Err(IpmiError::Parse("empty sdr chunk".into()));
                        }
                        rec.extend_from_slice(got);
                        // total はヘッダ到着後に確定するのでここで再計算
                        if rec.len() >= 5 {
                            let total = 5 + rec[4] as usize;
                            if rec.len() >= total {
                                rec.truncate(total);
                                break;
                            }
                        }
                    }
                    Err(IpmiError::Completion(0xc5)) => {
                        reservation_lost = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if reservation_lost {
                break;
            }
            records.push(rec);
            id = next_id;
        }
        if !reservation_lost {
            return Ok(records);
        }
    }
    Err(IpmiError::Parse("sdr reservation repeatedly lost".into()))
}

// ---- FRU ----

/// FRU の `offset` から `want` バイト読む。実際に返ったバイト列を返す。
fn read_fru_chunk(
    dev: &mut impl Transport,
    fru_id: u8,
    offset: usize,
    want: u8,
) -> Result<Vec<u8>> {
    let req = [fru_id, (offset & 0xff) as u8, (offset >> 8) as u8, want];
    let d = dev.request(NETFN_STORAGE, CMD_READ_FRU_DATA, &req)?;
    if d.is_empty() {
        return Err(IpmiError::Parse("empty fru chunk".into()));
    }
    let n = (d[0] as usize).min(d.len() - 1);
    Ok(d[1..1 + n].to_vec())
}

/// FRU デバイスの Product Info Area から Product Name を読む。
/// 全域を読まず common header → product area のみに絞る
/// （エリアが大きい FRU では数十KBの転送になるのを避ける）。
fn fru_product_name(dev: &mut impl Transport, fru_id: u8) -> Result<Option<String>> {
    // サイズ取得はデバイス存在確認も兼ねる（不在なら cc!=0 で Err）
    let info = dev.request(NETFN_STORAGE, CMD_GET_FRU_AREA_INFO, &[fru_id])?;
    if info.len() < 2 {
        return Err(IpmiError::Parse("short fru info".into()));
    }
    if info[0] == 0 && info[1] == 0 {
        return Ok(None);
    }
    let hdr = read_fru_chunk(dev, fru_id, 0, 8)?;
    if hdr.len() < 8 {
        return Ok(None);
    }
    // common header [4] = product area offset ×8（0 = エリアなし）
    let pa = hdr[4] as usize * 8;
    if pa == 0 {
        return Ok(None);
    }
    // product area: [0]=ver, [1]=len×8, [2]=lang, then type/len fields
    let pa_hdr = read_fru_chunk(dev, fru_id, pa, 3)?;
    if pa_hdr.len() < 3 {
        return Ok(None);
    }
    let area_len = pa_hdr[1] as usize * 8;
    if area_len < 3 {
        return Ok(None);
    }
    let area = read_fru_chunk(dev, fru_id, pa, area_len.min(255) as u8)?;
    Ok(parse_product_area(&area))
}

/// Product Info Area（先頭3バイト header 込み）から Product Name を
/// 取り出す。フィールド順: 0=manufacturer, 1=product name。
fn parse_product_area(area: &[u8]) -> Option<String> {
    let mut pos = 3;
    for idx in 0..=1u8 {
        let tl = *area.get(pos)?;
        pos += 1;
        if tl == 0xc1 {
            return None;
        }
        let len = (tl & 0x3f) as usize;
        let field = area.get(pos..pos + len)?;
        pos += len;
        if idx == 1 {
            return decode_id_string(tl, field).filter(|s| !s.is_empty());
        }
    }
    None
}

/// FRU デバイスを順に探し、最初に Product Name が取れたものを返す
/// （TX1320 M4 では FRU 2 = Chassis）。
/// 不在 FRU の completion error だけを許容して走査継続。
/// Io/Timeout/Parse は BMC 障害なので即失敗 — 全 FRU ×
/// タイムアウトを待って起動が数十秒遅れるのを防ぐ。
fn first_product_name(d: &mut impl Transport) -> Result<String> {
    let mut last_err = None;
    let mut any_ok = false;
    for fru_id in 0..MAX_FRU_DEVICES {
        match fru_product_name(d, fru_id) {
            Ok(Some(name)) => return Ok(name),
            Ok(None) => any_ok = true,
            Err(e @ IpmiError::Completion(_)) => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    if any_ok {
        return Err(IpmiError::Parse("no FRU product name found".into()));
    }
    match last_err {
        Some(e) => Err(e),
        None => Err(IpmiError::Parse("no FRU product name found".into())),
    }
}

// ---- バックエンド ----

pub struct NativeBackend {
    dev: Arc<Mutex<Device>>,
    /// パース済み Full Sensor Record のキャッシュ（取得時刻つき）。
    /// ポーリング毎の全レコード再走査（デバイスロック保持時間）を避ける。
    /// SENSOR_CACHE_TTL 経過か読み取りエラーで再スキャンする
    /// （PSU 増設・BMC FW 更新への追従経路）。
    sensors: Mutex<Option<(Arc<Vec<FullSensor>>, Instant)>>,
    path: PathBuf,
}

impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBackend")
            .field("path", &self.path)
            .finish()
    }
}

impl NativeBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            dev: Arc::new(Mutex::new(Device::open(path.as_ref())?)),
            sensors: Mutex::new(None),
            path: path.as_ref().to_path_buf(),
        })
    }

    /// ブロッキングな `Device` 操作を spawn_blocking で非同期化する。
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Device) -> Result<T> + Send + 'static,
    {
        let dev = Arc::clone(&self.dev);
        tokio::task::spawn_blocking(move || {
            // ポイズン時も回復する — クロージャのパニックがあっても
            // Device の ioctl 整合状態（msgid・受信キュー照合）は
            // 壊れないため、以後永久に IPMI 不能になる必要はない
            let mut dev = dev.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut dev)
        })
        .await
        .map_err(|e| IpmiError::Parse(format!("join error: {e}")))?
    }

    /// 生の netfn/cmd 要求（OEM コマンド用）。
    async fn request(&self, netfn: u8, cmd: u8, data: Vec<u8>) -> Result<Vec<u8>> {
        self.run(move |d| d.request(netfn, cmd, &data)).await
    }

    /// `Get Device ID` — probe 表示用。
    pub async fn device_id(&self) -> Result<DeviceId> {
        let d = self
            .request(NETFN_APP, CMD_GET_DEVICE_ID, Vec::new())
            .await?;
        DeviceId::parse(&d).ok_or_else(|| IpmiError::Parse("short device id response".into()))
    }

    /// SDR を走査して Full Sensor Record を全件パースする。
    /// 成功したスキャン結果はキャッシュされ、各ポーリング周期での
    /// 全レコード再走査（デバイスロック保持時間）を避ける。
    /// TTL 経過後は再走査する。
    async fn sensors(&self) -> Result<Arc<Vec<FullSensor>>> {
        {
            let cache = self.sensors.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((s, ts)) = cache.as_ref() {
                if ts.elapsed() < SENSOR_CACHE_TTL {
                    return Ok(Arc::clone(s));
                }
            }
        }
        let sensors = Arc::new(
            self.run(|d| {
                Ok(scan_sdr(d)?
                    .iter()
                    .filter_map(|r| parse_full_sensor(r))
                    .collect::<Vec<_>>())
            })
            .await?,
        );
        *self.sensors.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Arc::clone(&sensors), Instant::now()));
        Ok(sensors)
    }

    /// 読み取り系でトランスポートエラーが起きたらセンサーキャッシュを
    /// 捨て、次回ポーリングで SDR を再走査する（BMC 側のセンサー
    /// 集合が変化した場合への回復経路）。
    fn invalidate_sensors(&self) {
        *self.sensors.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// `Get Device ID` の応答（要部のみ）。
#[derive(Debug)]
pub struct DeviceId {
    pub device_id: u8,
    pub fw_major: u8,
    pub fw_minor: u8,
    pub ipmi_version: u8,
    pub manufacturer_id: u32,
    pub product_id: u16,
}

impl DeviceId {
    fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 11 {
            return None;
        }
        Some(Self {
            device_id: d[0],
            fw_major: d[2] & 0x7f,
            fw_minor: d[3],
            ipmi_version: d[4],
            manufacturer_id: d[6] as u32 | (d[7] as u32) << 8 | (d[8] as u32) << 16,
            product_id: d[9] as u16 | (d[10] as u16) << 8,
        })
    }
}

impl FanControlBackend for NativeBackend {
    fn model_name(&self) -> impl std::future::Future<Output = Result<String>> + Send {
        async move { self.run(first_product_name).await }
    }

    fn fans(&self) -> impl std::future::Future<Output = Result<Vec<FanReading>>> + Send {
        async move {
            let sensors = self.sensors().await?;
            let result = self
                .run(move |dev| {
                    let mut out = Vec::new();
                    for s in sensors
                        .iter()
                        .filter(|s| s.sensor_type == SENSOR_TYPE_FAN && !s.name.is_empty())
                    {
                        // トランスポート層のエラーは Disabled に潰さず伝播させる。
                        // ここで Err を返さないとデーモンの fan_failures /
                        // ipmi_failure_limit フェイルセーフが働かない。
                        // 値を欠測してよいのは BMC が明示的に返す
                        // Unavailable（completion code・unavailable bit）だけ。
                        let reading = match get_sensor_reading(dev, s.sensor_num)? {
                            SensorReading::Value { raw, thr } => {
                                // しきい値比較ビットが立っていれば Alarm
                                let alarm = s.event_type == 0x01 && thr & 0x3f != 0;
                                match linearize(s, raw) {
                                    Some(v) => FanReading {
                                        name: s.name.clone(),
                                        rpm: Some(v.max(0.0) as u32),
                                        status: if alarm {
                                            FanStatus::Alarm
                                        } else {
                                            FanStatus::Ok
                                        },
                                    },
                                    // 非線形/非アナログは値を信用できない
                                    // ので欠測（ipmitool の ns 相当）
                                    None => FanReading {
                                        name: s.name.clone(),
                                        rpm: None,
                                        status: FanStatus::Disabled,
                                    },
                                }
                            }
                            SensorReading::Unavailable => FanReading {
                                name: s.name.clone(),
                                rpm: None,
                                status: FanStatus::Disabled,
                            },
                        };
                        out.push(reading);
                    }
                    Ok(out)
                })
                .await;
            if result.is_err() {
                self.invalidate_sensors();
            }
            result
        }
    }

    fn temperatures(&self) -> impl std::future::Future<Output = Result<Vec<TempReading>>> + Send {
        async move {
            let sensors = self.sensors().await?;
            let result = self
                .run(move |dev| {
                    let mut out = Vec::new();
                    for s in sensors.iter().filter(|s| {
                        s.sensor_type == SENSOR_TYPE_TEMPERATURE
                            // unit が °C 以外（°F=0x02 等）のセンサーは
                            // 値をそのまま °C として扱えないので除外
                            && s.unit_base == UNIT_DEGREES_C
                            && !s.name.is_empty()
                    }) {
                        // fans() と同様、トランスポートエラーは伝播。
                        // 欠測として捨ててよいのは Unavailable と
                        // 非線形センサーだけ
                        if let SensorReading::Value { raw, .. } =
                            get_sensor_reading(dev, s.sensor_num)?
                        {
                            if let Some(c) = linearize(s, raw) {
                                out.push(TempReading {
                                    chip: "ipmi".into(),
                                    label: s.name.clone(),
                                    celsius: c,
                                });
                            }
                        }
                    }
                    Ok(out)
                })
                .await;
            if result.is_err() {
                self.invalidate_sensors();
            }
            result
        }
    }

    fn set_pwm(
        &self,
        scope: PwmScope,
        pwm: u8,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            if pwm > fujitsu::MAX_PWM {
                return Err(IpmiError::Parse(format!("pwm must be 0..=100, got {pwm}")));
            }
            let scope = match scope {
                PwmScope::All => fujitsu::SCOPE_ALL,
                PwmScope::Chassis => fujitsu::SCOPE_CHASSIS,
            };
            self.request(
                fujitsu::NETFN,
                fujitsu::COMMAND,
                fujitsu::set_pwm_data(scope, pwm),
            )
            .await?;
            Ok(())
        }
    }

    fn clear_override(&self) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            self.request(
                fujitsu::NETFN,
                fujitsu::COMMAND,
                fujitsu::clear_override_data(),
            )
            .await?;
            Ok(())
        }
    }

    fn read_override_slots(
        &self,
        indices: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<crate::backend::PwmSlot>>> + Send {
        let indices = indices.to_vec();
        async move {
            let data = fujitsu::read_slots_data(&indices)?;
            let resp = self.request(fujitsu::NETFN, fujitsu::COMMAND, data).await?;
            Ok(fujitsu::decode_slots(&resp, &indices))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実機 (TX1320 M4) の `sdr dump` から取得した FAN CPU の Full Sensor Record。
    /// M=25, B=0, exps=0 → rpm = raw * 25。raw=125 → 3125 RPM。
    /// （宣言長 rec[4]=0x3b → 計64B、末尾は BMC の 0 パディング）
    const REC_FAN_CPU: &[u8] = &[
        0x19, 0x00, 0x51, 0x01, 0x3b, 0x20, 0x00, 0x19, 0x1d, 0x00, 0x3b, 0x54, 0x04, 0x01, 0x00,
        0x20, 0x00, 0x00, 0x02, 0x02, 0x20, 0x12, 0x00, 0x00, 0x19, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0xff, 0xff, 0xff, 0x00, 0x18, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x81, 0xc7, 0x46, 0x41, 0x4e, 0x20, 0x43, 0x50, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    /// Ambient 温度: M=25, B=0, R_exp=-2 → celsius = raw * 25 / 100。
    /// raw=120 → 30.0 ℃。
    const REC_AMBIENT: &[u8] = &[
        0x01, 0x00, 0x51, 0x01, 0x3b, 0x20, 0x00, 0x01, 0x37, 0x00, 0x3b, 0x54, 0x01, 0x01, 0x85,
        0x32, 0x85, 0x32, 0x1b, 0x1b, 0x00, 0x01, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x00, 0xe0,
        0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0xc0, 0xb8, 0x00, 0x04, 0x10, 0x07, 0x03, 0x00,
        0x00, 0x00, 0xc7, 0x41, 0x6d, 0x62, 0x69, 0x65, 0x6e, 0x74, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    /// FAN PSU1: M=80 → rpm = raw * 80。raw=47 → 3760 RPM。
    const REC_FAN_PSU1: &[u8] = &[
        0x1d, 0x00, 0x51, 0x01, 0x3b, 0x20, 0x00, 0x24, 0x0a, 0x04, 0x3b, 0xd4, 0x04, 0x01, 0x00,
        0x20, 0x00, 0x00, 0x02, 0x02, 0x20, 0x12, 0x00, 0x00, 0x50, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0xff, 0xff, 0xff, 0x00, 0x0c, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x81, 0xc8, 0x46, 0x41, 0x4e, 0x20, 0x50, 0x53, 0x55, 0x31, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn parse_fan_cpu() {
        let s = parse_full_sensor(REC_FAN_CPU).unwrap();
        assert_eq!(s.sensor_num, 0x19);
        assert_eq!(s.sensor_type, SENSOR_TYPE_FAN);
        assert_eq!(s.unit_base, 0x12); // RPM
        assert_eq!(s.linearization, 0);
        assert_eq!(s.m, 25);
        assert_eq!(s.b, 0);
        assert_eq!(s.r_exp, 0);
        assert_eq!(s.b_exp, 0);
        assert_eq!(s.name, "FAN CPU");
        assert_eq!(linearize(&s, 125), Some(3125.0));
    }

    #[test]
    fn parse_fan_psu1_different_m() {
        let s = parse_full_sensor(REC_FAN_PSU1).unwrap();
        assert_eq!(s.name, "FAN PSU1");
        assert_eq!(s.m, 80);
        assert_eq!(s.r_exp, 0);
        assert_eq!(linearize(&s, 47), Some(3760.0));
    }

    #[test]
    fn parse_ambient_negative_r_exp() {
        let s = parse_full_sensor(REC_AMBIENT).unwrap();
        assert_eq!(s.name, "Ambient");
        assert_eq!(s.sensor_type, SENSOR_TYPE_TEMPERATURE);
        assert_eq!(s.m, 25);
        assert_eq!(s.b, 0);
        assert_eq!(s.r_exp, -2);
        assert!((linearize(&s, 120).unwrap() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn signed_raw_reading() {
        let mut s = parse_full_sensor(REC_AMBIENT).unwrap();
        s.analog_fmt = 2; // 2's complement
        s.m = 1;
        s.r_exp = 0;
        assert_eq!(linearize(&s, 0xfe), Some(-2.0));
        assert_eq!(linearize(&s, 0x20), Some(32.0));
    }

    #[test]
    fn non_linear_sensor_has_no_value() {
        let mut s = parse_full_sensor(REC_AMBIENT).unwrap();
        s.linearization = 0x07; // 1/x
        assert!(linearize(&s, 100).is_none());
        s.linearization = 0;
        s.analog_fmt = 3; // no analog reading
        assert!(linearize(&s, 100).is_none());
    }

    #[test]
    fn decode_6bit_packed_ascii() {
        // type=2、len はパック後の **バイト数**（4文字→3バイト）
        let data = [0xa1, 0x38, 0x92]; // 'A','B','C','D' (ch-0x20 packed)
        assert_eq!(decode_id_string(0x83, &data).as_deref(), Some("ABCD"));
        // 後続のパディングは length バイトで切り捨てる
        let padded = [0xa1, 0x38, 0x92, 0x00, 0x00];
        assert_eq!(decode_id_string(0x83, &padded).as_deref(), Some("ABCD"));
    }

    #[test]
    fn decode_6bit_packed_full_name() {
        // "FAN PSU1" = 8文字 → 6バイト。バイト数を文字数と
        // 誤解すると "FAN PS" に切れてセンサー名照合が壊れる。
        let data = [0x66, 0xe8, 0x02, 0xf0, 0x5c, 0x47];
        assert_eq!(decode_id_string(0x86, &data).as_deref(), Some("FAN PSU1"));
    }

    #[test]
    fn sensor_reading_flags_byte() {
        // d = [raw, flags, thr?]。flags: bit7=events on, bit6=scanning
        // on, bit5=reading unavailable。実機の健全値は 0xc0。
        assert!(matches!(
            parse_sensor_reading(&[0x3e, 0xc0, 0xc0]).unwrap(),
            SensorReading::Value {
                raw: 0x3e,
                thr: 0xc0
            }
        ));
        // scanning on のみでも有効（イベント無効でも読み取りは生きている）
        assert!(matches!(
            parse_sensor_reading(&[0x3e, 0x40]).unwrap(),
            SensorReading::Value { .. }
        ));
        // scanning disabled (bit6=0) → ラッチ済みの値を欠測扱い
        assert!(matches!(
            parse_sensor_reading(&[0x3e, 0x00]).unwrap(),
            SensorReading::Unavailable
        ));
        assert!(matches!(
            parse_sensor_reading(&[0x3e, 0x80]).unwrap(),
            SensorReading::Unavailable
        ));
        // reading unavailable (bit5)
        assert!(matches!(
            parse_sensor_reading(&[0x3e, 0x60]).unwrap(),
            SensorReading::Unavailable
        ));
        // 短い応答はパースエラー（欠測ではなく伝播）
        assert!(parse_sensor_reading(&[0x3e]).is_err());
        assert!(parse_sensor_reading(&[]).is_err());
    }

    #[test]
    fn decode_ascii() {
        assert_eq!(
            decode_id_string(0xc7, b"FAN CPU").as_deref(),
            Some("FAN CPU")
        );
        assert_eq!(decode_id_string(0x00, b"anything"), None);
    }

    #[test]
    fn product_area_parsed() {
        // product area: ver, len×8, lang, then fields
        let mut area = vec![0u8; 32];
        area[0] = 0x01;
        area[1] = 0x04; // 32 bytes
        area[2] = 0x00;
        area[3] = 0xc7; // type3 len7: manufacturer "FUJITSU"
        area[4..11].copy_from_slice(b"FUJITSU");
        area[11] = 0xd2; // type3 len18: product name
        area[12..30].copy_from_slice(b"PRIMERGY TX1320 M4");
        area[30] = 0xc1; // end of fields
        assert_eq!(
            parse_product_area(&area).as_deref(),
            Some("PRIMERGY TX1320 M4")
        );
    }

    #[test]
    fn device_id_parsed() {
        // 実機相当: fw 3.31, ipmi 2.0, mfg 0x002880 (Fujitsu), product 0x0501
        let d = [
            0x20, 0x00, 0x03, 0x31, 0x02, 0xbf, 0x80, 0x28, 0x00, 0x01, 0x05, 0x00, 0x00, 0x00,
            0x00,
        ];
        let id = DeviceId::parse(&d).unwrap();
        assert_eq!(id.manufacturer_id, 0x002880);
        assert_eq!(id.product_id, 0x0501);
        assert_eq!(id.fw_major, 0x03);
        assert_eq!(id.fw_minor, 0x31);
    }

    // ---- mock transport ----

    /// 要求を逐一出しで応答するスクリプト済みトランスポート。
    /// completion code は Device::request が剥がすので、ここでは
    /// `Err(IpmiError::Completion)` として直接スクリプトする。
    struct MockTransport {
        script: std::collections::VecDeque<(u8, u8, Result<Vec<u8>>)>,
        log: Vec<(u8, u8, Vec<u8>)>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                script: std::collections::VecDeque::new(),
                log: Vec::new(),
            }
        }
        fn respond(mut self, netfn: u8, cmd: u8, data: &[u8]) -> Self {
            self.script.push_back((netfn, cmd, Ok(data.to_vec())));
            self
        }
        fn fail(mut self, netfn: u8, cmd: u8, e: IpmiError) -> Self {
            self.script.push_back((netfn, cmd, Err(e)));
            self
        }
        /// 全スクリプトを消費したか確認する
        fn assert_done(&self) {
            assert!(self.script.is_empty(), "unconsumed script entries");
        }
    }

    impl Transport for MockTransport {
        fn request(&mut self, netfn: u8, cmd: u8, data: &[u8]) -> Result<Vec<u8>> {
            self.log.push((netfn, cmd, data.to_vec()));
            let (en, ec, resp) = self
                .script
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected request {netfn:#x}/{cmd:#x}"));
            assert_eq!((netfn, cmd), (en, ec), "request mismatch");
            resp
        }
    }

    /// Get SDR 応答 = [next_id lo, next_id hi, ...record bytes]
    fn sdr_resp(next_id: u16, rec: &[u8]) -> Vec<u8> {
        let mut v = vec![next_id as u8, (next_id >> 8) as u8];
        v.extend_from_slice(rec);
        v
    }

    #[test]
    fn sensor_reading_completion_whitelist() {
        // 実機確認済み: 不在センサーは 0xcb、不在センサー領域は 0xcd。
        // これらだけが Unavailable で、それ以外の completion code は
        // 全て伝播（0xcb 丸め込みはしない）
        for code in [0xcb, 0xcd] {
            let mut t = MockTransport::new().fail(0x04, 0x2d, IpmiError::Completion(code));
            assert!(matches!(
                get_sensor_reading(&mut t, 1),
                Ok(SensorReading::Unavailable)
            ));
        }
        for code in [0xc0, 0xc1, 0xc3, 0xd4, 0xff] {
            let mut t = MockTransport::new().fail(0x04, 0x2d, IpmiError::Completion(code));
            assert!(
                matches!(get_sensor_reading(&mut t, 1), Err(IpmiError::Completion(c)) if c == code)
            );
        }
    }

    #[test]
    fn scan_sdr_detects_id_cycle() {
        // BMC が next_id = id (自己ループ) を返す異常系:
        // デバイスロックを握ったまま無限巡回しないことを検証
        let rec = REC_FAN_CPU;
        let mut t = MockTransport::new()
            // Reserve SDR → reservation id 0x0001
            .respond(0x0a, 0x22, &[0x01, 0x00])
            // id=0 レコード: header + body、next=0x0005
            .respond(0x0a, 0x23, &sdr_resp(0x0005, &rec[..5]))
            .respond(0x0a, 0x23, &sdr_resp(0x0005, &rec[5..]))
            // id=5 レコード: next=0x0005 (自身) → 循環検出
            .respond(0x0a, 0x23, &sdr_resp(0x0005, &rec[..5]))
            .respond(0x0a, 0x23, &sdr_resp(0x0005, &rec[5..]));
        let err = scan_sdr(&mut t).unwrap_err();
        assert!(err.to_string().contains("chain broken"), "{err}");
    }

    #[test]
    fn scan_sdr_terminates_at_ffff() {
        let rec = REC_FAN_CPU;
        let mut t = MockTransport::new()
            .respond(0x0a, 0x22, &[0x01, 0x00])
            .respond(0x0a, 0x23, &sdr_resp(0xffff, &rec[..5]))
            .respond(0x0a, 0x23, &sdr_resp(0xffff, &rec[5..]));
        let records = scan_sdr(&mut t).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].len(), rec.len());
        t.assert_done();
    }

    #[test]
    fn first_product_name_skips_absent_fru() {
        // fru 0/1 は不在 (0xcb)、fru 2 で product name を返す。
        // product area: [ver, len×8, lang, field0=manufacturer "FJD",
        // field1=product name "TX132", 0xc1 end, pad, chk]
        let mut area = vec![0u8; 24];
        area[0] = 0x01;
        area[1] = 0x03; // 24 bytes
        area[2] = 0x00;
        area[3] = 0xc3;
        area[4..7].copy_from_slice(b"FJD");
        area[7] = 0xc5;
        area[8..13].copy_from_slice(b"TX132");
        area[13] = 0xc1;
        let chk = area.iter().skip(2).fold(0u8, |a, &b| a.wrapping_add(b));
        area[23] = 0u8.wrapping_sub(chk);
        // common header: [0x01, 0,0,0, product=1(x8B), 0, 0, chk]
        let hdr = [0x01u8, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0xfe];
        let mut area_read = vec![area.len() as u8];
        area_read.extend_from_slice(&area);
        let mut hdr_read = vec![hdr.len() as u8];
        hdr_read.extend_from_slice(&hdr);

        let mut t = MockTransport::new()
            .fail(0x0a, 0x10, IpmiError::Completion(0xcb)) // fru 0
            .fail(0x0a, 0x10, IpmiError::Completion(0xcb)) // fru 1
            .respond(0x0a, 0x10, &[0x20, 0x00, 0x00]) // fru 2: size 32
            .respond(0x0a, 0x11, &hdr_read) // common header @0
            .respond(0x0a, 0x11, &[0x03, 0x01, 0x03, 0x00]) // area hdr @8
            .respond(0x0a, 0x11, &area_read); // product area @8
        assert_eq!(first_product_name(&mut t).unwrap(), "TX132");
        t.assert_done();
    }

    #[test]
    fn first_product_name_propagates_transport_error() {
        // Io は不在 FRU とは区別して即失敗（走査を続けない）
        let mut t = MockTransport::new().fail(
            0x0a,
            0x10,
            IpmiError::Io(std::io::Error::from_raw_os_error(libc::EIO)),
        );
        assert!(matches!(first_product_name(&mut t), Err(IpmiError::Io(_))));
        t.assert_done();
    }
}
