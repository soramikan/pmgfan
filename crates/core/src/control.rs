//! 制御ループで使うレートリミッタ（ヒステリシス）。
//! docs/internals/04-control.md の方針: 冷却方向は速く、静音方向はゆっくり。

use serde::{Deserialize, Serialize};

/// PWM 強制の適用範囲（iRMC `W` コマンドのスコープバイト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PwmScope {
    /// 全ファン（PSU を含む）。設定値 `all`
    All,
    /// シャーシファン（FAN CPU / FANx SYS）のみ。
    /// PSU は iRMC 自動制御に残る。設定値 `chassis`
    Chassis,
}

impl PwmScope {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(Self::All),
            "chassis" => Ok(Self::Chassis),
            other => Err(format!(
                "unknown pwm_scope '{other}' (expected \"all\" or \"chassis\")"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Chassis => "chassis",
        }
    }
}

/// レート制限パラメータ。
#[derive(Debug, Clone, Copy)]
pub struct ControlParams {
    pub min_pwm: u8,
    pub max_pwm: u8,
    /// 強制 PWM の適用範囲
    pub pwm_scope: PwmScope,
    /// 1適用あたりの上昇幅
    pub step_up: u8,
    /// 1適用あたりの下降幅
    pub step_down: u8,
    /// 下降を開始するために必要な差（target が current より
    /// この値以上低いときだけ下降を始める）
    pub down_hysteresis: u8,
}

impl Default for ControlParams {
    fn default() -> Self {
        Self {
            min_pwm: 30,
            max_pwm: 100,
            pwm_scope: PwmScope::All,
            step_up: 20,
            step_down: 5,
            down_hysteresis: 5,
        }
    }
}

/// PWM 目標値を、上昇は速く・下降はゆっくり追従する値に整形する。
/// 書き込み周期そのものは呼び出し側（適用間隔 tick）が制御する。
///
/// 下降のヒステリシス: 目標との差が `down_hysteresis` 未満の間は
/// 下降を開始しないが、一度下降を始めたら目標に届くまで継続する
/// （`80 → 75 → 70 → 65 ...`）。
#[derive(Debug)]
pub struct RateLimiter {
    params: ControlParams,
    /// 現在適用中の PWM（直近に書いた値）
    current: Option<u8>,
    /// 下降シーケンス中か。下降開始後は deadband に入っても継続する。
    descending: bool,
}

impl RateLimiter {
    pub fn new(params: ControlParams) -> Self {
        Self {
            params,
            current: None,
            descending: false,
        }
    }

    /// モード切替時に内部状態をリセットする。
    pub fn reset(&mut self) {
        self.current = None;
        self.descending = false;
    }

    /// 実際に適用中の PWM をシードする。
    /// モード変更直後に `shared.pwm` の実値を入れることで、
    /// 次の `next()` が「初回適用」として目標値に直行せず、
    /// 現在値からの滑らかな変化になる。
    pub fn prime(&mut self, current: u8) {
        self.current = Some(current.clamp(self.params.min_pwm, self.params.max_pwm));
        self.descending = false;
    }

    /// 現在の適用値（まだ一度も書いていなければ None）。
    pub fn current(&self) -> Option<u8> {
        self.current
    }

    /// 目標 `target` に対する次の適用値を返す。
    /// 変更不要（ヒステリシス内・差分なし）なら `None`。
    pub fn next(&mut self, target: u8) -> Option<u8> {
        let t = target.clamp(self.params.min_pwm, self.params.max_pwm);
        let cur = match self.current {
            None => {
                self.current = Some(t);
                return Some(t);
            }
            Some(c) => c,
        };
        let next = if t > cur {
            // 上昇は即応（冷却優先）
            self.descending = false;
            cur.saturating_add(self.params.step_up).min(t)
        } else if cur > t {
            if !self.descending && cur - t < self.params.down_hysteresis {
                return None;
            }
            let n = cur.saturating_sub(self.params.step_down).max(t);
            self.descending = n > t;
            n
        } else {
            self.descending = false;
            return None;
        };
        if next == cur {
            None
        } else {
            self.current = Some(next);
            Some(next)
        }
    }
}

/// PI 制御パラメータ（`[target_rpm]` セクション由来）。
#[derive(Debug, Clone, Copy)]
pub struct PiParams {
    /// PWM% per RPM 誤差（比例項）
    pub kp: f32,
    /// PWM% per RPM·tick 誤差積分（積分項）
    pub ki: f32,
    /// |誤差| がこの値以下なら PWM を変えない（振動防止）
    pub deadband_rpm: f32,
    pub min_pwm: u8,
    pub max_pwm: u8,
}

/// Target RPM 用 PI コントローラ。
/// `pwm += kp*err + ki*∫err` で、参照ファンの実測 RPM を
/// 目標値へ追従させる。D 項はファン制御では不要（docs/internals/04-control.md）。
///
/// アンチワインドアップは条件付き積分: 出力が限界に張り付き、
/// かつ誤差がさらに同じ方向へ積み増そうとするときだけ
/// 積分を更新しない（限界から復帰する方向の積分は許可する）。
#[derive(Debug)]
pub struct PiController {
    params: PiParams,
    /// 内部の連続 PWM 値（端数を蓄積するため float）
    pwm: f32,
    /// 誤差積分
    integral: f32,
}

impl PiController {
    pub fn new(params: PiParams) -> Self {
        Self {
            params,
            pwm: params.min_pwm as f32,
            integral: 0.0,
        }
    }

    /// モード開始時の初期 PWM（現在適用値 or キャリブレーション
    /// 推定値）をシードする。積分はリセットされる。
    pub fn prime(&mut self, pwm: f32) {
        self.pwm = pwm.clamp(self.params.min_pwm as f32, self.params.max_pwm as f32);
        self.integral = 0.0;
    }

    /// 実測 `measured_rpm` が目標 `target_rpm` に近づくよう
    /// 次の PWM を返す。deadband 内なら現在値を維持する。
    pub fn next(&mut self, measured_rpm: f32, target_rpm: f32) -> u8 {
        let (min, max) = (self.params.min_pwm as f32, self.params.max_pwm as f32);
        let err = target_rpm - measured_rpm;
        if err.abs() <= self.params.deadband_rpm {
            return self.pwm.round().clamp(min, max) as u8;
        }
        let candidate = self.pwm + self.params.kp * err + self.params.ki * (self.integral + err);
        let clamped = candidate.clamp(min, max);
        // 出力が飽和し、誤差がさらに飽和方向へ向かうときだけ積分しない
        let saturated_up = candidate > max && err > 0.0;
        let saturated_down = candidate < min && err < 0.0;
        if !(saturated_up || saturated_down) {
            self.integral += err;
        }
        self.pwm = clamped;
        clamped.round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        RateLimiter::new(ControlParams::default())
    }

    #[test]
    fn first_apply_takes_clamped_target() {
        let mut l = limiter();
        assert_eq!(l.next(40), Some(40));
        let mut l = limiter();
        assert_eq!(l.next(10), Some(30)); // min_pwm にクランプ
    }

    #[test]
    fn ramp_up_is_fast() {
        let mut l = limiter();
        l.next(30);
        // 30→80: step_up=20 で 50 に進む
        assert_eq!(l.next(80), Some(50));
        assert_eq!(l.next(80), Some(70));
        assert_eq!(l.next(80), Some(80));
        assert_eq!(l.next(80), None);
    }

    #[test]
    fn ramp_down_is_slow_with_hysteresis() {
        let mut l = limiter();
        l.next(80);
        // 差が hysteresis(5) 未満 → 下降を開始しない
        assert_eq!(l.next(76), None);
        // 差が hysteresis に達すると下降開始（80-5=75 で target 到達）
        assert_eq!(l.next(75), Some(75));
        // 以降は step_down=5 ずつ、目標まで継続
        assert_eq!(l.next(60), Some(70));
        assert_eq!(l.next(60), Some(65));
        assert_eq!(l.next(60), Some(60));
        assert_eq!(l.next(60), None);
        // 目標は min_pwm にクランプされたうえで判定される
        let mut l3 = limiter();
        l3.next(40);
        assert_eq!(l3.next(20), Some(35)); // target 20 → 30 にクランプ、40-30>=5 で 35 へ
        assert_eq!(l3.next(30), Some(30)); // 下降継続で目標に到達
        assert_eq!(l3.next(30), None);
    }

    fn pi() -> PiController {
        PiController::new(PiParams {
            kp: 0.01, // テストしやすい大きめのゲイン
            ki: 0.001,
            deadband_rpm: 50.0,
            min_pwm: 10,
            max_pwm: 100,
        })
    }

    #[test]
    fn pi_increases_pwm_when_rpm_below_target() {
        let mut c = pi();
        c.prime(40.0);
        // 目標 2500 に対して実測 2000（誤差 +500）
        let out = c.next(2000.0, 2500.0);
        assert!(out > 40, "below target must raise pwm, got {out}");
        // 反対方向
        let mut c = pi();
        c.prime(40.0);
        let out = c.next(3000.0, 2500.0);
        assert!(out < 40, "above target must lower pwm, got {out}");
    }

    #[test]
    fn pi_deadband_holds_pwm() {
        let mut c = pi();
        c.prime(40.0);
        assert_eq!(c.next(2475.0, 2500.0), 40); // err=25 < deadband 50
        assert_eq!(c.next(2525.0, 2500.0), 40);
    }

    #[test]
    fn pi_converges_and_stops_integrating_at_saturation() {
        let mut c = pi();
        c.prime(40.0);
        // 目標にずっと届かない状況で上限に張り付く。
        // 初回 tick はまだ立ち上がり途中なので、十分な回数を回して
        // 飽和したあと「飽和が維持される」ことを確認する
        for _ in 0..200 {
            c.next(500.0, 3000.0);
        }
        for _ in 0..50 {
            assert_eq!(c.next(500.0, 3000.0), 100);
        }
        // 飽和中に積分が暴れないこと: 復帰方向の誤差ですぐ降り始める
        let out = c.next(4000.0, 3000.0);
        assert!(out < 100, "should unwind quickly, got {out}");
    }

    #[test]
    fn pi_clamps_output() {
        let mut c = pi();
        c.prime(50.0);
        assert!(c.next(0.0, 20000.0) <= 100);
        assert!(c.next(20000.0, 500.0) >= 10);
    }

    #[test]
    fn pi_prime_clamps_and_resets_integral() {
        let mut c = pi();
        c.prime(150.0); // 範囲外 → max にクランプ
        assert_eq!(c.next(2500.0, 2500.0), 100);
        c.prime(5.0); // min にクランプ
        assert_eq!(c.next(2500.0, 2500.0), 10);
    }
}
