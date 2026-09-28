//! ファンカーブ。温度 → PWM の線形補間。docs/internals/04-control.md 参照。

/// カーブの1点。`temp` ℃ で `pwm` %。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurvePoint {
    pub temp: f32,
    pub pwm: f32,
}

/// センサー1つ分のカーブ。
#[derive(Debug, Clone)]
pub struct Curve {
    /// センサー名（解決は `sensor::resolve` が行う）
    pub sensor: String,
    /// `temp` 昇順の制御点
    pub points: Vec<CurvePoint>,
}

#[derive(Debug, thiserror::Error)]
pub enum CurveError {
    #[error("curve '{sensor}' needs at least 2 points")]
    TooFewPoints { sensor: String },
    #[error("curve '{sensor}' point {index}: non-finite value")]
    NonFinite { sensor: String, index: usize },
    #[error("curve '{sensor}' point {index}: pwm {pwm} out of range 0..=100")]
    PwmOutOfRange {
        sensor: String,
        index: usize,
        pwm: f32,
    },
    #[error("curve '{sensor}' point {index}: temperatures must be strictly increasing")]
    NonMonotonic { sensor: String, index: usize },
}

impl Curve {
    /// 検証つきコンストラクタ。`points` は `(temp, pwm)` 列。
    pub fn new(sensor: impl Into<String>, points: Vec<(f32, f32)>) -> Result<Self, CurveError> {
        let c = Self {
            sensor: sensor.into(),
            points: points
                .into_iter()
                .map(|(temp, pwm)| CurvePoint { temp, pwm })
                .collect(),
        };
        c.validate()?;
        Ok(c)
    }

    /// 設定ファイル適用前の検証。
    /// - 2点以上
    /// - 温度は厳密に昇順
    /// - PWM は 0..=100 の有限値
    pub fn validate(&self) -> Result<(), CurveError> {
        if self.points.len() < 2 {
            return Err(CurveError::TooFewPoints {
                sensor: self.sensor.clone(),
            });
        }
        for (i, p) in self.points.iter().enumerate() {
            if !p.temp.is_finite() || !p.pwm.is_finite() {
                return Err(CurveError::NonFinite {
                    sensor: self.sensor.clone(),
                    index: i,
                });
            }
            if !(0.0..=100.0).contains(&p.pwm) {
                return Err(CurveError::PwmOutOfRange {
                    sensor: self.sensor.clone(),
                    index: i,
                    pwm: p.pwm,
                });
            }
            if i > 0 && self.points[i - 1].temp >= p.temp {
                return Err(CurveError::NonMonotonic {
                    sensor: self.sensor.clone(),
                    index: i,
                });
            }
        }
        Ok(())
    }

    /// 線形補間で `temp` ℃ → PWM %。範囲外は端点にクランプする。
    /// 非有限値入力は壊れたセンサーとして安全側（末端点 = 通常最大値）を返す。
    pub fn eval(&self, temp: f32) -> f32 {
        let pts = &self.points;
        debug_assert!(!pts.is_empty());
        if pts.is_empty() {
            return 0.0;
        }
        if !temp.is_finite() {
            return pts[pts.len() - 1].pwm;
        }
        if temp <= pts[0].temp {
            return pts[0].pwm;
        }
        for w in pts.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if temp <= b.temp {
                let f = (temp - a.temp) / (b.temp - a.temp);
                return a.pwm + f * (b.pwm - a.pwm);
            }
        }
        pts[pts.len() - 1].pwm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_curve() -> Curve {
        Curve::new(
            "cpu_package",
            vec![(35.0, 30.0), (50.0, 35.0), (60.0, 45.0), (90.0, 100.0)],
        )
        .unwrap()
    }

    #[test]
    fn eval_clamps_endpoints() {
        let c = cpu_curve();
        assert_eq!(c.eval(10.0), 30.0);
        assert_eq!(c.eval(35.0), 30.0);
        assert_eq!(c.eval(90.0), 100.0);
        assert_eq!(c.eval(120.0), 100.0);
    }

    #[test]
    fn eval_interpolates() {
        let c = cpu_curve();
        // 35..50 で 30..35 → 42.5℃ は中点
        assert!((c.eval(42.5) - 32.5).abs() < 1e-6);
        // 50℃ でちょうど 35
        assert_eq!(c.eval(50.0), 35.0);
    }

    #[test]
    fn eval_non_finite_temp_goes_to_last_point() {
        let c = cpu_curve();
        // 壊れたセンサー入力は安全側（最大要求）に倒す
        assert_eq!(c.eval(f32::NAN), 100.0);
        assert_eq!(c.eval(f32::INFINITY), 100.0);
    }

    #[test]
    fn validate_rejects_bad_curves() {
        assert!(matches!(
            Curve::new("x", vec![(10.0, 30.0)]),
            Err(CurveError::TooFewPoints { .. })
        ));
        assert!(matches!(
            Curve::new("x", vec![(50.0, 30.0), (40.0, 40.0)]),
            Err(CurveError::NonMonotonic { .. })
        ));
        assert!(matches!(
            Curve::new("x", vec![(10.0, 30.0), (50.0, 120.0)]),
            Err(CurveError::PwmOutOfRange { .. })
        ));
        assert!(matches!(
            Curve::new("x", vec![(10.0, 30.0), (f32::NAN, 40.0)]),
            Err(CurveError::NonFinite { .. })
        ));
    }
}
