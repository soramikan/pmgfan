//! 制御ループで使うレートリミッタ（ヒステリシス）。
//! docs/03-control.md の方針: 冷却方向は速く、静音方向はゆっくり。

/// レート制限パラメータ。
#[derive(Debug, Clone, Copy)]
pub struct ControlParams {
    pub min_pwm: u8,
    pub max_pwm: u8,
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
}
