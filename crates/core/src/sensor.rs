//! 温度センサーの読み取りモデル。

use serde::{Deserialize, Serialize};

/// 温度1点分の読み取り結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempReading {
    /// 取得元。hwmon チップ名（`coretemp` 等）または `ipmi`
    pub chip: String,
    /// センサー名。hwmon の `tempN_label` または IPMI センサー名
    pub label: String,
    pub celsius: f64,
}

/// 論理名 → 実センサー候補（`chip/label` 形式、先勝ち）。
/// hwmon ラベルが無い入力は `tempN_input` → `tempN` 正規化で照合する。
const ALIASES: &[(&str, &[&str])] = &[
    ("cpu_package", &["coretemp/package id 0", "ipmi/cpu"]),
    ("cpu", &["ipmi/cpu", "coretemp/package id 0"]),
    ("pch", &["ipmi/pch", "pch_cannonlake/temp1"]),
    ("ambient", &["ipmi/ambient"]),
    (
        "nvme",
        &["nvme/composite", "ipmi/pciessd sff", "ipmi/m.2 ssd"],
    ),
];

/// 設定上のセンサー名を `temps` 中の `TempReading` に解決する。
///
/// 解決順:
/// 1. 上記エイリアスの候補を順に探索
/// 2. `chip/label`（または `chip/tempN` 正規形）完全一致
/// 3. `label` 完全一致
///
/// 大文字小文字は区別しない。
pub fn resolve<'a>(temps: &'a [TempReading], name: &str) -> Option<&'a TempReading> {
    let key = name.to_lowercase();
    if let Some((_, candidates)) = ALIASES.iter().find(|(k, _)| *k == key) {
        for cand in *candidates {
            if let Some(t) = temps.iter().find(|t| matches(t, cand)) {
                return Some(t);
            }
        }
        return None;
    }
    temps.iter().find(|t| matches(t, &key))
}

fn matches(t: &TempReading, key: &str) -> bool {
    let chip = t.chip.to_lowercase();
    let label = t.label.to_lowercase();
    let stripped = label.strip_suffix("_input").unwrap_or(&label);
    key == label
        || key == format!("{chip}/{label}")
        || key == format!("{chip}/{stripped}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<TempReading> {
        vec![
            TempReading {
                chip: "ipmi".into(),
                label: "CPU".into(),
                celsius: 36.0,
            },
            TempReading {
                chip: "ipmi".into(),
                label: "PCH".into(),
                celsius: 53.0,
            },
            TempReading {
                chip: "coretemp".into(),
                label: "Package id 0".into(),
                celsius: 37.0,
            },
            TempReading {
                chip: "pch_cannonlake".into(),
                label: "temp1_input".into(),
                celsius: 52.0,
            },
        ]
    }

    #[test]
    fn resolves_aliases() {
        let temps = sample();
        assert_eq!(resolve(&temps, "cpu_package").unwrap().celsius, 37.0); // hwmon 優先
        assert_eq!(resolve(&temps, "cpu").unwrap().celsius, 36.0); // ipmi 優先
        assert_eq!(resolve(&temps, "pch").unwrap().celsius, 53.0);
        assert!(resolve(&temps, "nvme").is_none());
    }

    #[test]
    fn resolves_chip_label_and_normalized_names() {
        let temps = sample();
        assert_eq!(
            resolve(&temps, "coretemp/Package id 0").unwrap().celsius,
            37.0
        );
        // tempN_input → tempN 正規化
        assert_eq!(
            resolve(&temps, "pch_cannonlake/temp1").unwrap().celsius,
            52.0
        );
        assert_eq!(
            resolve(&temps, "pch_cannonlake/temp1_input").unwrap().celsius,
            52.0
        );
        assert_eq!(resolve(&temps, "Package id 0").unwrap().celsius, 37.0);
    }
}
