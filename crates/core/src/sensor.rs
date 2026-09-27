//! 温度センサーの読み取りモデル。

/// 温度1点分の読み取り結果。
#[derive(Debug, Clone, PartialEq)]
pub struct TempReading {
    /// 取得元。hwmon チップ名（`coretemp` 等）または `ipmi`
    pub chip: String,
    /// センサー名。hwmon の `tempN_label` または IPMI センサー名
    pub label: String,
    pub celsius: f64,
}
