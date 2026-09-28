//! Ratatui TUI。pmgfand の状態監視・モード切替を行う。
//! TUI を閉じてもデーモン側の制御は継続する（ビューアであり、
//! 制御本体ではない）。

use std::collections::{HashMap, VecDeque};
use std::io::stdout;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use pmgfan_core::config::Config;
use pmgfan_core::control::PwmScope;
use pmgfan_core::curve::Curve;
use pmgfan_core::fan::FanStatus;
use pmgfan_core::protocol::{CurveSpec, DaemonState, Mode, Request, Response};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Sparkline};
use ratatui::{Frame, Terminal};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use crate::client;

/// 状態ポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// ポーリング要求のタイムアウト（応答タスク側。ループはブロックしない）。
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// モード変更要求のタイムアウト。
const REQ_TIMEOUT: Duration = Duration::from_secs(10);
/// キー入力待ちの上限（これにより描画が周期的に回る）。
const TICK: Duration = Duration::from_millis(100);
/// 温度スパークラインの保持サンプル数。
const HISTORY_LEN: usize = 180;
/// notice の表示時間。
const NOTICE_TTL: Duration = Duration::from_secs(4);
/// Fixed PWM ダイアログの下限/上限デフォルト（config 未指定時。
/// daemon 側既定 30 / 物理上限 100 に合わせる）。
const DEFAULT_PWM_MIN: u8 = 30;
const DEFAULT_PWM_MAX: u8 = 100;
/// 描画に必要な最小端末サイズ。
const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 22;
/// カーブパネルを出すのに必要な高さ。
const CURVE_MIN_HEIGHT: u16 = 32;

/// 最後に取得したデーモン状態。
#[derive(Debug)]
struct Status {
    state: DaemonState,
    mode: Mode,
    pwm: Option<u8>,
    pwm_scope: PwmScope,
    fans: Vec<pmgfan_core::fan::FanReading>,
    temps: Vec<pmgfan_core::sensor::TempReading>,
    uptime_secs: f64,
}

/// バックグラウンド要求タスクからの結果。
enum Outcome {
    Status(Result<Status, String>),
    ModeSet {
        label: String,
        result: Result<(), String>,
    },
    /// 起動時・保存後に取得するライブカーブ一覧
    CurvesFetched(Result<Vec<CurveSpec>, String>),
    /// カーブ保存（SetCurves）の結果
    CurvesSaved(Result<(), String>),
    /// スコープ切替（SetPwmScope）の結果
    ScopeSet {
        scope: PwmScope,
        result: Result<(), String>,
    },
}

/// カーブエディタの状態。`curves` は全カーブの編集用コピー。
struct CurveEditor {
    /// (sensor 名, 制御点 (temp, pwm) の列)
    curves: Vec<(String, Vec<(f32, f32)>)>,
    /// 編集中のカーブ index（Tab で切替）
    curve_idx: usize,
    /// 選択中の制御点 index
    sel: usize,
    /// 未保存の変更があるか
    dirty: bool,
}

struct App {
    socket: PathBuf,
    curves: Vec<Curve>,
    /// Fixed PWM ダイアログの下限/上限（config の control.* 由来）。
    pwm_min: u8,
    pwm_max: u8,
    /// PWM 強制スコープ。起動時は config 値、
    /// ポーリング成功後はデーモンのランタイム値に追従する
    pwm_scope: PwmScope,
    status: Option<Status>,
    conn_error: Option<String>,
    /// `chip/label` → 温度履歴
    temp_history: HashMap<String, VecDeque<u64>>,
    pwm_dialog: Option<u8>,
    /// カーブエディタ表示中は Some
    editor: Option<CurveEditor>,
    notice: Option<(Instant, String)>,
    quit: bool,
    tx: mpsc::UnboundedSender<Outcome>,
    rx: mpsc::UnboundedReceiver<Outcome>,
    /// ポーリング要求が飛行中か（多重ポーリング防止）。
    poll_inflight: bool,
    /// モード変更要求が飛行中か（並行 SetMode 防止）。
    req_inflight: bool,
    /// 最後にポーリングを開始した時刻。
    last_poll: Instant,
}

impl App {
    fn new(
        socket: PathBuf,
        curves: Vec<Curve>,
        pwm_min: u8,
        pwm_max: u8,
        pwm_scope: PwmScope,
        tx: mpsc::UnboundedSender<Outcome>,
        rx: mpsc::UnboundedReceiver<Outcome>,
    ) -> Self {
        Self {
            socket,
            curves,
            pwm_min,
            pwm_max,
            pwm_scope,
            status: None,
            conn_error: None,
            temp_history: HashMap::new(),
            pwm_dialog: None,
            editor: None,
            notice: None,
            quit: false,
            tx,
            rx,
            poll_inflight: false,
            req_inflight: false,
            // 初回ポーリングを即座に起こすため過去時刻にする
            last_poll: Instant::now()
                .checked_sub(POLL_INTERVAL)
                .unwrap_or_else(Instant::now),
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((Instant::now(), msg.into()));
    }

    /// 状態ポーリングをバックグラウンドで開始する。
    /// イベントループをブロックしない。
    fn spawn_refresh(&mut self) {
        self.poll_inflight = true;
        self.last_poll = Instant::now();
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r =
                tokio::time::timeout(POLL_TIMEOUT, client::request(&socket, &Request::GetStatus))
                    .await;
            let outcome = match r {
                Ok(Ok(Response::Status {
                    state,
                    mode,
                    pwm,
                    pwm_scope,
                    fans,
                    temperatures,
                    uptime_secs,
                })) => Outcome::Status(Ok(Status {
                    state,
                    mode,
                    pwm,
                    pwm_scope,
                    fans,
                    temps: temperatures,
                    uptime_secs,
                })),
                Ok(Ok(Response::Error { error })) => Outcome::Status(Err(error)),
                Ok(Ok(_)) => Outcome::Status(Err("unexpected response".into())),
                Ok(Err(e)) => Outcome::Status(Err(e.to_string())),
                Err(_) => Outcome::Status(Err("request timeout".into())),
            };
            let _ = tx.send(outcome);
        });
    }

    /// モード変更要求をバックグラウンドで送信する。
    /// 前の要求が飛行中なら無視する（デーモン側の適用順と
    /// キー押下順が逆転するのを防ぐ）。
    fn set_mode(&mut self, mode: Mode, label: &str) {
        if self.req_inflight {
            self.notify("a request is already in flight");
            return;
        }
        self.req_inflight = true;
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        let label = label.to_string();
        tokio::spawn(async move {
            let r = tokio::time::timeout(
                REQ_TIMEOUT,
                client::request(&socket, &Request::SetMode { mode }),
            )
            .await;
            let result = match r {
                Ok(Ok(resp)) => client::expect_ok(resp).map_err(|e| format!("{e:#}")),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("request timeout".into()),
            };
            let _ = tx.send(Outcome::ModeSet { label, result });
        });
    }

    /// デーモンが現在使用中のカーブ一覧を取得する
    /// （config ファイルではなくランタイム状態が正）。
    fn spawn_get_curves(&mut self) {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r =
                tokio::time::timeout(REQ_TIMEOUT, client::request(&socket, &Request::GetCurves))
                    .await;
            let outcome = match r {
                Ok(Ok(Response::Curves { curves })) => Outcome::CurvesFetched(Ok(curves)),
                Ok(Ok(Response::Error { error })) => Outcome::CurvesFetched(Err(error)),
                Ok(Ok(_)) => Outcome::CurvesFetched(Err("unexpected response".into())),
                Ok(Err(e)) => Outcome::CurvesFetched(Err(e.to_string())),
                Err(_) => Outcome::CurvesFetched(Err("request timeout".into())),
            };
            let _ = tx.send(outcome);
        });
    }

    /// エディタの内容を SetCurves で保存（検証・永続化は
    /// デーモン側が行う）。保存中は req_inflight を立てる。
    fn save_curves(&mut self) {
        let Some(ed) = &self.editor else {
            return;
        };
        if self.req_inflight {
            self.notify("a request is already in flight");
            return;
        }
        self.req_inflight = true;
        let specs: Vec<CurveSpec> = ed
            .curves
            .iter()
            .map(|(sensor, points)| CurveSpec {
                sensor: sensor.clone(),
                points: points.clone(),
            })
            .collect();
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = tokio::time::timeout(
                REQ_TIMEOUT,
                client::request(&socket, &Request::SetCurves { curves: specs }),
            )
            .await;
            let result = match r {
                Ok(Ok(resp)) => client::expect_ok(resp).map_err(|e| format!("{e:#}")),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("request timeout".into()),
            };
            let _ = tx.send(Outcome::CurvesSaved(result));
        });
    }

    /// PWM スコープを all ↔ chassis でトグルする。
    /// SetCurves と同じく検証・永続化・適用はデーモン側。
    fn toggle_scope(&mut self) {
        if self.req_inflight {
            self.notify("a request is already in flight");
            return;
        }
        let scope = match self.pwm_scope {
            PwmScope::All => PwmScope::Chassis,
            PwmScope::Chassis => PwmScope::All,
        };
        self.req_inflight = true;
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = tokio::time::timeout(
                REQ_TIMEOUT,
                client::request(&socket, &Request::SetPwmScope { scope }),
            )
            .await;
            let result = match r {
                Ok(Ok(resp)) => client::expect_ok(resp).map_err(|e| format!("{e:#}")),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("request timeout".into()),
            };
            let _ = tx.send(Outcome::ScopeSet { scope, result });
        });
    }

    /// 要求タスクの結果を状態へ反映する。
    fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Status(Ok(s)) => {
                self.poll_inflight = false;
                self.conn_error = None;
                // 消えたセンサーの履歴を捨てる
                self.temp_history.retain(|k, _| {
                    s.temps
                        .iter()
                        .any(|t| format!("{}/{}", t.chip, t.label) == *k)
                });
                for t in &s.temps {
                    if !t.celsius.is_finite() {
                        continue;
                    }
                    let key = format!("{}/{}", t.chip, t.label);
                    let hist = self.temp_history.entry(key).or_default();
                    hist.push_back((t.celsius * 10.0).clamp(0.0, 1000.0) as u64);
                    while hist.len() > HISTORY_LEN {
                        hist.pop_front();
                    }
                }
                // スコープはデーモンのランタイム値を正とする
                // （config ファイル値は接続前の初期表示用）
                self.pwm_scope = s.pwm_scope;
                self.status = Some(s);
            }
            Outcome::Status(Err(e)) => {
                self.poll_inflight = false;
                self.conn_error = Some(e);
            }
            Outcome::ModeSet { label, result } => {
                self.req_inflight = false;
                match result {
                    Ok(()) => {
                        self.notify(format!("mode applied: {label}"));
                        // 直後の画面更新のため即座に再ポーリング
                        // （spawn_refresh が last_poll も更新する）
                        if !self.poll_inflight {
                            self.spawn_refresh();
                        }
                    }
                    Err(e) => self.notify(e),
                }
            }
            Outcome::CurvesFetched(Ok(specs)) => {
                // デーモン側のランタイムカーブを表示用の正とする。
                // 壊れた spec は落とす（daemon は送出前に検証済みの
                // はずだが、防御的に）。
                self.curves = specs
                    .iter()
                    .filter_map(|s| Curve::new(s.sensor.clone(), s.points.clone()).ok())
                    .collect();
            }
            Outcome::CurvesFetched(Err(e)) => {
                self.notify(format!("get curves failed: {e}"));
            }
            Outcome::CurvesSaved(result) => {
                self.req_inflight = false;
                match result {
                    Ok(()) => {
                        self.editor = None;
                        self.notify("curves saved");
                        self.spawn_get_curves();
                        if !self.poll_inflight {
                            self.spawn_refresh();
                        }
                    }
                    // 検証・永続化失敗はエディタを開いたままにして
                    // 修正し直せるようにする
                    Err(e) => self.notify(format!("save failed: {e}")),
                }
            }
            Outcome::ScopeSet { scope, result } => {
                self.req_inflight = false;
                match result {
                    Ok(()) => {
                        self.pwm_scope = scope;
                        let note = match scope {
                            PwmScope::Chassis => "chassis fans only (PSU: iRMC auto)",
                            PwmScope::All => "all fans incl. PSU",
                        };
                        self.notify(format!("pwm scope -> {note}"));
                        if !self.poll_inflight {
                            self.spawn_refresh();
                        }
                    }
                    Err(e) => self.notify(format!("scope change failed: {e}")),
                }
            }
        }
    }

    /// 溜まった Outcome を全て処理する。
    fn drain_outcomes(&mut self) {
        while let Ok(o) = self.rx.try_recv() {
            self.apply_outcome(o);
        }
    }
}

/// TUI を実行する。戻るときは端末を元の状態に復元する。
pub async fn run(socket: &Path, config_path: Option<&Path>) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("pmgfanctl tui requires an interactive terminal");
    }
    let cfg = load_config(config_path);

    // panic 時は代替画面にメッセージが飲まれないよう、
    // 端末を復元してから既定フックへ委譲する。
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        prev_hook(info);
    }));

    enable_raw_mode()?;
    // この時点以降の失敗・panic はガードの Drop で復元される。
    let _guard = TerminalGuard;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    // 外部シグナル（kill 等）でもループを抜けて Drop で復元する。
    let stop = Arc::new(AtomicBool::new(false));
    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        let stop = stop.clone();
        tokio::spawn(async move {
            // 登録失敗・recv が None を返した場合は
            // 終了扱いにしない（単にシグナル無しで動作する）
            if let Ok(mut s) = signal(kind) {
                if s.recv().await.is_some() {
                    stop.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let scope = PwmScope::parse(&cfg.pwm_scope).unwrap_or(PwmScope::All);
    let mut app = App::new(
        socket.to_path_buf(),
        cfg.curves,
        cfg.pwm_min,
        cfg.pwm_max,
        scope,
        tx,
        rx,
    );
    if let Some(e) = cfg.error {
        app.notify(format!("config: {e}"));
    }
    // ライブカーブをデーモンから取得（config ファイルは
    // 接続できるまでのフォールバック表示用）
    app.spawn_get_curves();

    // 初回ポーリングはバックグラウンドなので、即座に
    // "connecting" 画面を描画できる（last_poll は
    // App::new で過去時刻に初期化済み）。
    while !app.quit && !stop.load(Ordering::SeqCst) {
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key.code, key.modifiers);
                }
            }
        }
        app.drain_outcomes();
        if !app.poll_inflight && app.last_poll.elapsed() >= POLL_INTERVAL {
            app.spawn_refresh();
        }
        // 古い notice を消す
        if app
            .notice
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() > NOTICE_TTL)
        {
            app.notice = None;
        }
    }
    Ok(())
}

/// スコープ終了時に端末を復元するガード（panic 時も Drop で戻す）。
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

/// 設定ファイルから TUI 用の値を読み込む（表示専用。
/// 読めない場合はデフォルトで動作し、理由は error に入れる）。
struct TuiConfig {
    curves: Vec<Curve>,
    pwm_min: u8,
    pwm_max: u8,
    /// 強制 PWM の適用範囲（"all"/"chassis"）
    pwm_scope: String,
    error: Option<String>,
}

fn load_config(config_path: Option<&Path>) -> TuiConfig {
    let mut cfg = TuiConfig {
        curves: Vec::new(),
        pwm_min: DEFAULT_PWM_MIN,
        pwm_max: DEFAULT_PWM_MAX,
        pwm_scope: "all".into(),
        error: None,
    };
    let Some(path) = config_path else {
        return cfg;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            cfg.error = Some(format!("cannot read {}: {e}", path.display()));
            return cfg;
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(config) => {
            // config の min_pwm をそのまま下限に使う（30 未満も
            // 設定可能。daemon 側でも同じ値で検証される）
            cfg.pwm_max = config.control.max_pwm.min(100);
            cfg.pwm_min = config.control.min_pwm.min(cfg.pwm_max);
            cfg.pwm_scope = config.control.pwm_scope.clone();
            cfg.curves = config
                .curves
                .iter()
                .filter_map(|c| Curve::new(c.sensor.clone(), c.points.clone()).ok())
                .collect();
        }
        Err(e) => cfg.error = Some(format!("cannot parse {}: {e}", path.display())),
    }
    cfg
}

/// Fixed PWM ダイアログの値を [min, max] に収める。
fn clamp_pwm(pwm: u8, delta: i8, min: u8, max: u8) -> u8 {
    pwm.saturating_add_signed(delta).clamp(min.min(max), max)
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    // Ctrl 修飾付き文字キーはモード操作に使わない。
    // raw mode では Ctrl+C が SIGINT にならず Char('c')+CONTROL
    // として届くため、誤ってモード変更へ割り当てない。
    if modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(code, KeyCode::Char('c') | KeyCode::Char('C')) {
            app.quit = true;
        }
        return;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        return;
    }

    if app.editor.is_some() {
        handle_editor_key(app, code, modifiers);
        return;
    }

    if let Some(pwm) = app.pwm_dialog {
        let step: i8 = if modifiers.contains(KeyModifiers::SHIFT) {
            5
        } else {
            1
        };
        match code {
            KeyCode::Left => app.pwm_dialog = Some(clamp_pwm(pwm, -step, app.pwm_min, app.pwm_max)),
            KeyCode::Right => app.pwm_dialog = Some(clamp_pwm(pwm, step, app.pwm_min, app.pwm_max)),
            KeyCode::Enter => {
                app.pwm_dialog = None;
                app.set_mode(Mode::FixedPwm(pwm), &format!("fixed PWM {pwm}%"));
            }
            KeyCode::Esc => app.pwm_dialog = None,
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Esc => app.quit = true,
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'q' => app.quit = true,
            'a' => app.set_mode(Mode::IrmcAuto, "iRMC Auto"),
            'c' => app.set_mode(Mode::Curve, "Curve"),
            'f' => {
                let seed = app
                    .status
                    .as_ref()
                    .and_then(|s| match s.mode {
                        Mode::FixedPwm(p) => Some(p),
                        _ => s.pwm,
                    })
                    .unwrap_or(40);
                app.pwm_dialog = Some(clamp_pwm(seed, 0, app.pwm_min, app.pwm_max));
            }
            'r' => app.notify("target RPM mode is not implemented yet (phase 7)"),
            's' => app.toggle_scope(),
            'e' => {
                if app.curves.is_empty() {
                    app.notify("no fan curves configured");
                } else {
                    let curves = app
                        .curves
                        .iter()
                        .map(|c| {
                            (
                                c.sensor.clone(),
                                c.points.iter().map(|p| (p.temp, p.pwm)).collect::<Vec<_>>(),
                            )
                        })
                        .collect();
                    app.editor = Some(CurveEditor {
                        curves,
                        curve_idx: 0,
                        sel: 0,
                        dirty: false,
                    });
                }
            }
            'l' => app.notify("logs: see `journalctl -u pmgfand`"),
            _ => {}
        },
        _ => {}
    }
}

/// カーブエディタのキー処理。
/// ↑↓ 選択 / ←→ PWM ±1（Shift ±5）/ +,- 温度 ±1（Shift ±5）/
/// A 点追加 / D 点削除 / Tab カーブ切替 / S 保存 / Esc 取消。
fn handle_editor_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Some(mut ed) = app.editor.take() else {
        return;
    };
    let step: f32 = if modifiers.contains(KeyModifiers::SHIFT) {
        5.0
    } else {
        1.0
    };
    match code {
        KeyCode::Esc => {
            // editor は None のまま = 変更破棄
            return;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            ed.curve_idx = if code == KeyCode::Tab {
                (ed.curve_idx + 1) % ed.curves.len().max(1)
            } else {
                ed.curve_idx.checked_sub(1).unwrap_or(ed.curves.len() - 1)
            };
            ed.sel = 0;
        }
        KeyCode::Up => ed.sel = ed.sel.saturating_sub(1),
        KeyCode::Down => {
            let n = ed.curves[ed.curve_idx].1.len();
            ed.sel = (ed.sel + 1).min(n.saturating_sub(1));
        }
        KeyCode::Left | KeyCode::Right => {
            let dir = if code == KeyCode::Left { -1.0 } else { 1.0 };
            let pts = &mut ed.curves[ed.curve_idx].1;
            let (t, p) = pts[ed.sel];
            let np = (p + dir * step).clamp(app.pwm_min as f32, app.pwm_max as f32);
            if np != p {
                pts[ed.sel] = (t, np);
                ed.dirty = true;
            }
        }
        KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Char('+') | KeyCode::Char('=') => {
            let dir = if matches!(code, KeyCode::Char('-') | KeyCode::Char('_')) {
                -1.0
            } else {
                1.0
            };
            let pts = &mut ed.curves[ed.curve_idx].1;
            let (t, p) = pts[ed.sel];
            // 厳密昇順を維持するため隣接点の内側にクランプ
            let lo = if ed.sel > 0 {
                pts[ed.sel - 1].0 + 1.0
            } else {
                0.0
            };
            let hi = if ed.sel + 1 < pts.len() {
                pts[ed.sel + 1].0 - 1.0
            } else {
                150.0
            };
            let nt = (t + dir * step).clamp(lo.min(hi), hi);
            if nt != t {
                pts[ed.sel] = (nt, p);
                ed.dirty = true;
            }
        }
        KeyCode::Char('a') => {
            let pts = &mut ed.curves[ed.curve_idx].1;
            let cur = pts[ed.sel];
            // 選択点と次点の中間温度に挿入（末尾なら +10℃）
            let nt = if ed.sel + 1 < pts.len() {
                (cur.0 + pts[ed.sel + 1].0) / 2.0
            } else {
                (cur.0 + 10.0).min(150.0)
            };
            // 中点が作れない（隣接と同値以下）なら追加しない
            if nt > cur.0 && (ed.sel + 1 >= pts.len() || nt < pts[ed.sel + 1].0) {
                pts.insert(ed.sel + 1, (nt, cur.1));
                ed.sel += 1;
                ed.dirty = true;
            } else {
                app.notify("no room to insert a point here");
            }
        }
        KeyCode::Char('d') => {
            let pts = &mut ed.curves[ed.curve_idx].1;
            if pts.len() > 2 {
                pts.remove(ed.sel);
                ed.sel = ed.sel.min(pts.len() - 1);
                ed.dirty = true;
            } else {
                app.notify("a curve needs at least 2 points");
            }
        }
        KeyCode::Char('s') => {
            app.editor = Some(ed);
            app.save_curves();
            return;
        }
        _ => {}
    }
    app.editor = Some(ed);
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        f.render_widget(
            Paragraph::new(format!(
                "terminal too small ({}x{}; need at least {}x{})",
                area.width, area.height, MIN_WIDTH, MIN_HEIGHT
            ))
            .style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }

    // 高さが足りなければカーブパネルを省略して footer を守る。
    let show_curves = !app.curves.is_empty() && area.height >= CURVE_MIN_HEIGHT;
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(9), // fans
        Constraint::Min(8),    // temperatures
    ];
    if show_curves {
        constraints.push(Constraint::Length(10)); // curve
    }
    constraints.push(Constraint::Length(2)); // footer
    let chunks = Layout::vertical(constraints).split(area);

    draw_header(f, app, chunks[0]);
    draw_fans(f, app, chunks[1]);
    draw_temps(f, app, chunks[2]);
    let mut idx = 3;
    if show_curves {
        draw_curve(f, app, chunks[3]);
        idx = 4;
    }
    draw_footer(f, app, chunks[idx]);

    if let Some(pwm) = app.pwm_dialog {
        draw_pwm_dialog(f, pwm, app.pwm_min, app.pwm_max);
    }
    if let Some(ed) = &app.editor {
        draw_editor(f, ed, app.pwm_min);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (mode, state, pwm, uptime) = match &app.status {
        Some(s) => (
            mode_label(&s.mode),
            format!("{:?}", s.state),
            s.pwm
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "--".into()),
            format!("{}s", s.uptime_secs as u64),
        ),
        None => ("--".into(), "--".into(), "--".into(), "--".into()),
    };
    let conn = if let Some(e) = &app.conn_error {
        Span::styled(format!("  conn: {e}"), Style::default().fg(Color::Red))
    } else if app.status.is_none() {
        Span::styled("  conn: …", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled("  conn: OK", Style::default().fg(Color::Green))
    };
    let state_color = match app.status.as_ref().map(|s| s.state) {
        Some(DaemonState::Controlling | DaemonState::Monitoring) => Color::Green,
        Some(DaemonState::Failsafe | DaemonState::Degraded) => Color::Red,
        _ => Color::Gray,
    };
    let line = Line::from(vec![
        Span::styled(" Mode: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            mode,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   State: ", Style::default().fg(Color::DarkGray)),
        Span::styled(state, Style::default().fg(state_color)),
        Span::styled("   PWM: ", Style::default().fg(Color::DarkGray)),
        Span::styled(pwm, Style::default().fg(Color::Yellow)),
        // chassis スコープでは PSU が iRMC 自動制御に残る。
        // all では PSU ファンも強制対象になるため目立つ色で示す
        match app.pwm_scope {
            PwmScope::Chassis => Span::styled(" [chassis]", Style::default().fg(Color::DarkGray)),
            PwmScope::All => Span::styled(" [all+PSU]", Style::default().fg(Color::Yellow)),
        },
        Span::styled("   Up: ", Style::default().fg(Color::DarkGray)),
        Span::raw(uptime),
        conn,
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_fans(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Fans ").borders(Borders::ALL);
    let Some(status) = &app.status else {
        f.render_widget(Paragraph::new("no data").block(block), area);
        return;
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let max_rpm = status
        .fans
        .iter()
        .filter_map(|f| f.rpm)
        .max()
        .unwrap_or(1)
        .max(1) as usize;

    let visible = inner.height as usize;
    let mut lines: Vec<Line> = status
        .fans
        .iter()
        .take(visible)
        .map(|fan| {
            let bar_w = inner.width.saturating_sub(34).max(4) as usize;
            let (rpm_s, bar) = match fan.rpm {
                Some(rpm) => {
                    let filled = (rpm as usize).saturating_mul(bar_w) / max_rpm;
                    let mut b = "█".repeat(filled.min(bar_w));
                    b.push_str(&"░".repeat(bar_w - filled.min(bar_w)));
                    (format!("{rpm:>6} RPM"), b)
                }
                None => ("    --    ".to_string(), "░".repeat(bar_w)),
            };
            let color = match fan.status {
                FanStatus::Ok => Color::Green,
                FanStatus::Disabled | FanStatus::NotPresent => Color::DarkGray,
                FanStatus::Alarm => Color::Red,
                FanStatus::Unknown => Color::Yellow,
            };
            Line::from(vec![
                Span::styled(
                    format!(" {:<12}", fan.name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(rpm_s, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(bar, Style::default().fg(Color::Blue)),
                Span::raw("  "),
                Span::styled(fan.status.as_str(), Style::default().fg(color)),
            ])
        })
        .collect();
    // 表示しきれない分はインジケータで知らせる（Alarm の見落とし防止）。
    // 最終行をインジケータに譲るため、実際に隠れる数は hidden+1。
    let hidden = status.fans.len().saturating_sub(visible);
    if hidden > 0 && !lines.is_empty() {
        lines.pop();
        lines.push(Line::from(Span::styled(
            format!(" … +{} more", hidden + 1),
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_temps(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Temperatures ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(status) = &app.status else {
        f.render_widget(Paragraph::new("no data"), inner);
        return;
    };
    // センサーごとに [name] [temp] [sparkline] の行を並べる。
    // Layout::split は制約と同じ数の Rect を返すため
    // 切詰には使えない。高さから手動で切り、収まらない分は
    // インジケータを出す（draw_fans と同じ方式）。
    let visible = inner.height as usize;
    let total = status.temps.len();
    // 溢れる場合は最終行をインジケータに使う
    let shown = if total > visible {
        visible.saturating_sub(1)
    } else {
        total
    };
    for (i, t) in status.temps.iter().take(shown).enumerate() {
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let key = format!("{}/{}", t.chip, t.label);
        let hist: Vec<u64> = app
            .temp_history
            .get(&key)
            .map(|h| h.iter().copied().collect::<Vec<u64>>())
            .unwrap_or_default();
        let color = temp_color(t.celsius);
        let cells = Layout::horizontal([
            Constraint::Length(26),
            Constraint::Length(7),
            Constraint::Min(10),
        ])
        .split(row);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {:<10}", t.chip),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<13}", t.label),
                    Style::default().fg(Color::White),
                ),
            ])),
            cells[0],
        );
        let temp_s = if t.celsius.is_finite() {
            format!("{:>5.1}°C", t.celsius)
        } else {
            "    --°C".to_string()
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                temp_s,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
            cells[1],
        );
        f.render_widget(
            Sparkline::default()
                .data(&hist)
                .max(1000)
                .style(Style::default().fg(color)),
            cells[2],
        );
    }
    if shown < total && visible > 0 {
        let row = Rect {
            y: inner.y + shown as u16,
            height: 1,
            ..inner
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" … +{} more", total - shown),
                Style::default().fg(Color::DarkGray),
            ))),
            row,
        );
    }
}

fn draw_curve(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Fan Curves ").borders(Borders::ALL);
    // 制御点の温度範囲に合わせて X 軸を広げる（設定値が
    // 20..100 の外にはみ出しても見えるように）。
    let (mut lo, mut hi) = (20.0_f64, 100.0_f64);
    for c in &app.curves {
        for p in &c.points {
            lo = lo.min(p.temp as f64 - 5.0);
            hi = hi.max(p.temp as f64 + 5.0);
        }
    }
    let lo = lo.clamp(0.0, 90.0);
    let hi = hi.clamp(lo + 5.0, 120.0);
    let canvas = Canvas::default()
        .block(block)
        .x_bounds([lo, hi])
        .y_bounds([0.0, 105.0])
        .paint(|ctx| {
            for (i, curve) in app.curves.iter().enumerate() {
                // 折れ線 + 制御点
                for seg in curve.points.windows(2) {
                    ctx.draw(&CanvasLine {
                        x1: seg[0].temp as f64,
                        y1: seg[0].pwm as f64,
                        x2: seg[1].temp as f64,
                        y2: seg[1].pwm as f64,
                        color: curve_color(i),
                    });
                }
                for p in &curve.points {
                    ctx.draw(&Points {
                        coords: &[(p.temp as f64, p.pwm as f64)],
                        color: Color::White,
                    });
                }
                ctx.print(lo + 1.0, 100.0 - i as f64 * 6.0, curve.sensor.to_string());
            }
        });
    f.render_widget(canvas, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hint = Line::from(vec![
        Span::styled(" [A]", Style::default().fg(Color::Yellow)),
        Span::raw("uto "),
        Span::styled("[C]", Style::default().fg(Color::Yellow)),
        Span::raw("urve "),
        Span::styled("[F]", Style::default().fg(Color::Yellow)),
        Span::raw("ixed PWM "),
        Span::styled("[R]", Style::default().fg(Color::Yellow)),
        Span::raw("PM "),
        Span::styled("[E]", Style::default().fg(Color::Yellow)),
        Span::raw("dit "),
        Span::styled("[S]", Style::default().fg(Color::Yellow)),
        Span::raw("cope "),
        Span::styled("[L]", Style::default().fg(Color::Yellow)),
        Span::raw("ogs "),
        Span::styled("[Q]", Style::default().fg(Color::Yellow)),
        Span::raw("uit"),
    ]);
    let notice = app
        .notice
        .as_ref()
        .map(|(_, m)| Line::from(Span::styled(m.clone(), Style::default().fg(Color::Magenta))))
        .unwrap_or_else(|| Line::from(""));
    f.render_widget(Paragraph::new(vec![hint, notice]), area);
}

fn draw_pwm_dialog(f: &mut Frame, pwm: u8, pwm_min: u8, pwm_max: u8) {
    let area = centered_rect(30, 9, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(" Fixed PWM ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(format!("{pwm} %"))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(format!("←/→ ±1%   Shift+←/→ ±5%   ({pwm_min}..{pwm_max}%)"))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new("Enter: apply   Esc: cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

/// カーブエディタのモーダル。制御点テーブル + 簡易プレビュー。
fn draw_editor(f: &mut Frame, ed: &CurveEditor, pwm_min: u8) {
    let area = f.area();
    let (sensor, pts) = &ed.curves[ed.curve_idx];
    let w = 66u16.min(area.width.saturating_sub(4));
    // タイトル+ヘッダ+点数+空行+2行ヘルプ
    let want_h = pts.len() as u16 + 8;
    let h = want_h.clamp(8, area.height.saturating_sub(2));
    let dlg = centered_rect(w, h, area);
    f.render_widget(Clear, dlg);
    let title = format!(
        " Curve Editor — {sensor} ({}/{}){} ",
        ed.curve_idx + 1,
        ed.curves.len(),
        if ed.dirty { "  [unsaved]" } else { "" }
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dlg);
    f.render_widget(block, dlg);
    // 左: 制御点テーブル、右: プレビューグラフ
    let cols = Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).split(inner);
    // ヘッダ行 + 点数 + 空行 + ヘルプ2行
    let table_area = cols[0];
    let body_rows = (table_area.height as usize).saturating_sub(4);
    // 選択行が見えるようウィンドウ化
    let start = ed
        .sel
        .saturating_sub(body_rows.saturating_sub(1))
        .min(pts.len().saturating_sub(body_rows.max(1)));
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "     Temp    PWM",
        Style::default().fg(Color::DarkGray),
    ))];
    for (i, (t, p)) in pts.iter().enumerate().skip(start).take(body_rows.max(1)) {
        let sel = i == ed.sel;
        let style = if sel {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let marker = if sel { ">" } else { " " };
        let bar = "▮".repeat((*p / 10.0) as usize);
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {t:>5.1}°C  {p:>3.0}% "), style),
            Span::styled(bar, Style::default().fg(Color::Blue)),
        ]));
    }
    if start + body_rows < pts.len() {
        lines.push(Line::from(Span::styled(
            "   …",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ sel  ←→ pwm  +/- temp",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        " A add  D del  Tab curve  S save  Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), table_area);

    // 編集中の点列をそのままプレビュー
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for c in &ed.curves {
        for (t, _) in &c.1 {
            lo = lo.min(*t as f64);
            hi = hi.max(*t as f64);
        }
    }
    let lo = (lo - 5.0).clamp(0.0, 140.0);
    let hi = (hi + 5.0).clamp(lo + 10.0, 160.0);
    let canvas = Canvas::default()
        .x_bounds([lo, hi])
        .y_bounds([pwm_min as f64 - 10.0, 110.0])
        .paint(|ctx| {
            for (ci, (_, cpts)) in ed.curves.iter().enumerate() {
                let color = if ci == ed.curve_idx {
                    Color::Cyan
                } else {
                    Color::DarkGray
                };
                for seg in cpts.windows(2) {
                    ctx.draw(&CanvasLine {
                        x1: seg[0].0 as f64,
                        y1: seg[0].1 as f64,
                        x2: seg[1].0 as f64,
                        y2: seg[1].1 as f64,
                        color,
                    });
                }
                for (i, p) in cpts.iter().enumerate() {
                    let color = if ci == ed.curve_idx && i == ed.sel {
                        Color::Yellow
                    } else {
                        color
                    };
                    ctx.draw(&Points {
                        coords: &[(p.0 as f64, p.1 as f64)],
                        color,
                    });
                }
            }
        });
    f.render_widget(canvas, cols[1]);
}

fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

fn mode_label(mode: &Mode) -> String {
    match mode {
        Mode::IrmcAuto => "iRMC Auto".into(),
        Mode::FixedPwm(p) => format!("Fixed PWM {p}%"),
        Mode::Curve => "Curve".into(),
        Mode::TargetRpm { fan, rpm } => format!("Target {rpm} RPM ({fan})"),
    }
}

fn temp_color(c: f64) -> Color {
    if !c.is_finite() {
        Color::DarkGray
    } else if c >= 85.0 {
        Color::Red
    } else if c >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn curve_color(i: usize) -> Color {
    const COLORS: &[Color] = &[
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Green,
        Color::Blue,
        Color::Red,
    ];
    COLORS[i % COLORS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (tx, rx) = mpsc::unbounded_channel();
        App::new(
            PathBuf::from("/nonexistent.sock"),
            Vec::new(),
            DEFAULT_PWM_MIN,
            DEFAULT_PWM_MAX,
            PwmScope::All,
            tx,
            rx,
        )
    }

    #[test]
    fn clamp_pwm_respects_bounds() {
        assert_eq!(clamp_pwm(30, -1, 30, 100), 30);
        assert_eq!(clamp_pwm(100, 1, 30, 100), 100);
        assert_eq!(clamp_pwm(31, -5, 30, 100), 30);
        assert_eq!(clamp_pwm(99, 5, 30, 100), 100);
        assert_eq!(clamp_pwm(50, 1, 30, 100), 51);
        // 設定由来の狭い範囲
        assert_eq!(clamp_pwm(45, -10, 40, 80), 40);
        assert_eq!(clamp_pwm(75, 10, 40, 80), 80);
        // min > max の異常設定でも panic せず下限値に収まる
        assert_eq!(clamp_pwm(50, 1, 90, 30), 30);
    }

    #[test]
    fn clamp_pwm_seeds_out_of_range() {
        assert_eq!(clamp_pwm(10, 0, 30, 100), 30);
        assert_eq!(clamp_pwm(200, 0, 30, 100), 100);
        assert_eq!(clamp_pwm(200, -1, 30, 100), 100);
    }

    #[test]
    fn ctrl_c_quits_instead_of_mode_switch() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
        // Ctrl+<他のキー> は何も起きない（quit も set_mode も）
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(!app.quit);
        assert!(app.pwm_dialog.is_none());
    }

    #[test]
    fn alt_and_uppercase_keys() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::ALT);
        assert!(!app.quit);
        // 大文字・CapsLock でも quit できる
        handle_key(&mut app, KeyCode::Char('Q'), KeyModifiers::SHIFT);
        assert!(app.quit);
    }

    #[test]
    fn esc_quits_and_dialog_cancel() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
        assert!(app.pwm_dialog.is_some());
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.pwm_dialog.is_none());
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit);
    }

    #[test]
    fn dialog_uses_config_bounds() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            PathBuf::from("/x.sock"),
            Vec::new(),
            40,
            80,
            PwmScope::All,
            tx,
            rx,
        );
        // seed が範囲外でも開いた時点でクランプされる
        app.status = Some(Status {
            state: DaemonState::Controlling,
            mode: Mode::FixedPwm(20),
            pwm: Some(20),
            pwm_scope: PwmScope::All,
            fans: vec![],
            temps: vec![],
            uptime_secs: 0.0,
        });
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(app.pwm_dialog, Some(40));
        handle_key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(app.pwm_dialog, Some(45));
    }

    #[test]
    fn temp_color_ranges() {
        assert_eq!(temp_color(50.0), Color::Green);
        assert_eq!(temp_color(75.0), Color::Yellow);
        assert_eq!(temp_color(90.0), Color::Red);
        assert_eq!(temp_color(f64::NAN), Color::DarkGray);
        assert_eq!(temp_color(f64::INFINITY), Color::DarkGray);
    }

    #[test]
    fn centered_rect_stays_in_bounds() {
        let area = Rect::new(0, 0, 10, 5);
        let r = centered_rect(30, 9, area);
        assert!(r.width <= area.width);
        assert!(r.height <= area.height);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        let big = Rect::new(5, 5, 80, 24);
        let r = centered_rect(30, 9, big);
        assert_eq!(r, Rect::new(30, 12, 30, 9));
    }

    /// エディタ付きの App を作る
    fn editor_app() -> App {
        let mut app = test_app();
        app.curves = vec![Curve::new(
            "cpu_package",
            vec![(35.0, 30.0), (60.0, 45.0), (90.0, 100.0)],
        )
        .unwrap()];
        handle_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(app.editor.is_some());
        app
    }

    #[test]
    fn editor_opens_and_cancels() {
        let mut app = editor_app();
        let ed = app.editor.as_ref().unwrap();
        assert_eq!(ed.curves[0].1.len(), 3);
        assert!(!ed.dirty);
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.editor.is_none());
    }

    #[test]
    fn editor_pwm_and_temp_clamps() {
        let mut app = editor_app();
        // sel=0 (35℃,30%) → 下限側: 30 未満に下がらない
        for _ in 0..10 {
            handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        assert_eq!(app.editor.as_ref().unwrap().curves[0].1[0].1, 30.0);
        // 上限側: 100 超に上がらない
        for _ in 0..20 {
            handle_key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        assert_eq!(app.editor.as_ref().unwrap().curves[0].1[0].1, 100.0);
        // 温度を上げると次点(60℃)の1つ下まで
        for _ in 0..30 {
            handle_key(&mut app, KeyCode::Char('+'), KeyModifiers::SHIFT);
        }
        assert_eq!(app.editor.as_ref().unwrap().curves[0].1[0].0, 59.0);
        assert!(app.editor.as_ref().unwrap().dirty);
    }

    #[test]
    fn editor_add_delete_and_min_points() {
        let mut app = editor_app();
        // 最終点を選択して A → 中間点追加
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        let ed = app.editor.as_ref().unwrap();
        assert_eq!(ed.curves[0].1.len(), 4);
        assert_eq!(ed.sel, 3);
        // 昇順が保たれている
        let ts: Vec<f32> = ed.curves[0].1.iter().map(|p| p.0).collect();
        assert!(ts.windows(2).all(|w| w[0] < w[1]));
        // 2点までしか削除できない
        for _ in 0..10 {
            handle_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
        }
        assert_eq!(app.editor.as_ref().unwrap().curves[0].1.len(), 2);
    }

    #[test]
    fn mode_label_variants() {
        assert_eq!(mode_label(&Mode::IrmcAuto), "iRMC Auto");
        assert_eq!(mode_label(&Mode::FixedPwm(42)), "Fixed PWM 42%");
        assert_eq!(mode_label(&Mode::Curve), "Curve");
        assert_eq!(
            mode_label(&Mode::TargetRpm {
                fan: "FAN CPU".into(),
                rpm: 2500
            }),
            "Target 2500 RPM (FAN CPU)"
        );
    }
}
