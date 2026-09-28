//! Linux hwmon（`/sys/class/hwmon`）からの温度読み取り。
//!
//! `hwmonN/name` をチップ名、`hwmonN/tempM_input`（millidegree C）と
//! `hwmonN/tempM_label` を各センサーとして読む。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::sensor::TempReading;

/// 既定の hwmon ルート。
pub const HWMON_ROOT: &str = "/sys/class/hwmon";

/// `hwmon_dir`（通常 `/sys/class/hwmon`）配下の全温度センサーを読む。
///
/// 読み取りに失敗した個別センサーはスキップする。ディレクトリ自体が
/// 開けない場合のみ `Err` を返す。
pub fn read_temperatures(hwmon_dir: &Path) -> io::Result<Vec<TempReading>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(hwmon_dir)? {
        // 1エントリの失敗で全センサーを失わないよう skip する
        let Ok(entry) = entry else {
            continue;
        };
        let name_file = entry.path().join("name");
        let Ok(chip) = fs::read_to_string(&name_file) else {
            continue;
        };
        let chip = chip.trim().to_string();
        out.extend(read_chip(&entry.path(), &chip));
    }
    Ok(out)
}

fn read_chip(dir: &Path, chip: &str) -> Vec<TempReading> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        let Some(idx) = temp_input_index(fname) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(millideg) = raw.trim().parse::<f64>() else {
            continue;
        };
        let label = read_label(dir, idx).unwrap_or_else(|| fname.to_string());
        out.push(TempReading {
            chip: chip.to_string(),
            label,
            celsius: millideg / 1000.0,
        });
    }
    out
}

/// `temp12_input` → `Some(12)`。他のファイル名は `None`。
fn temp_input_index(fname: &str) -> Option<u32> {
    let rest = fname.strip_prefix("temp")?;
    let idx = rest.strip_suffix("_input")?;
    idx.parse().ok()
}

fn read_label(dir: &Path, idx: u32) -> Option<String> {
    let path: PathBuf = dir.join(format!("temp{idx}_label"));
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    #[test]
    fn temp_input_index_parses_only_temp_inputs() {
        assert_eq!(temp_input_index("temp1_input"), Some(1));
        assert_eq!(temp_input_index("temp12_input"), Some(12));
        assert_eq!(temp_input_index("temp1_label"), None);
        assert_eq!(temp_input_index("fan1_input"), None);
        assert_eq!(temp_input_index("name"), None);
        assert_eq!(temp_input_index("tempX_input"), None);
    }

    #[test]
    fn reads_chip_with_labels_and_unlabeled_inputs() {
        let root = std::env::temp_dir().join(format!("pmgfan-hwmon-test-{}", std::process::id()));
        let hwmon = root.join("hwmon0");
        create_dir_all(&hwmon).unwrap();
        write(hwmon.join("name"), "coretemp\n").unwrap();
        write(hwmon.join("temp1_input"), "41000\n").unwrap();
        write(hwmon.join("temp1_label"), "Package id 0\n").unwrap();
        write(hwmon.join("temp2_input"), "38500\n").unwrap();
        // 温度でない入力や壊れた値はスキップされる
        write(hwmon.join("fan1_input"), "1200\n").unwrap();
        write(hwmon.join("temp3_input"), "garbage\n").unwrap();

        let readings = read_temperatures(&root).unwrap();
        assert_eq!(readings.len(), 2);
        assert!(readings
            .iter()
            .any(|t| t.chip == "coretemp" && t.label == "Package id 0" && t.celsius == 41.0));
        assert!(readings
            .iter()
            .any(|t| t.chip == "coretemp" && t.label == "temp2_input" && t.celsius == 38.5));
        std::fs::remove_dir_all(&root).ok();
    }
}
