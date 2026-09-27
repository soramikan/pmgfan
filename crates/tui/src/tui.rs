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
use pmgfan_core::curve::Curve;
use pmgfan_core::fan::FanStatus;
use pmgfan_core::protocol::{DaemonState, Mode, Request, Response};
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
    fans: Vec<pmgfan_core::fan::FanReading>,
    temps: Vec<pmgfan_core::sensor::TempReading>,
    uptime_secs: f64,
}

/// バックグラウンド要求タスクからの結果。
enum Outcome {
    Status(Result<Status, String>),
    ModeSet { label: String, result: Result<(), String> },
}

struct App {
    socket: PathBuf,
    curves: Vec<Curve>,
    /// Fixed PWM ダイアログの下限/上限（config の control.* 由来）。
    pwm_min: u8,
    pwm_max: u8,
    status: Option<Status>,
    conn_error: Option<String>,
    /// `chip/label` → 温度履歴
    temp_history: HashMap<String, VecDeque<u64>>,
    pwm_dialog: Option<u8>,
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
        tx: mpsc::UnboundedSender<Outcome>,
        rx: mpsc::UnboundedReceiver<Outcome>,
    ) -> Self {
        Self {
            socket,
            curves,
            pwm_min,
            pwm_max,
            status: None,
            conn_error: None,
            temp_history: HashMap::new(),
            pwm_dialog: None,
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
            let r = tokio::time::timeout(
                POLL_TIMEOUT,
                client::request(&socket, &Request::GetStatus),
            )
            .await;
            let outcome = match r {
                Ok(Ok(Response::Status {
                    state,
                    mode,
                    pwm,
                    fans,
                    temperatures,
                    uptime_secs,
                })) => Outcome::Status(Ok(Status {
                    state,
                    mode,
                    pwm,
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

    /// 要求タスクの結果を状態へ反映する。
    fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Status(Ok(s)) => {
                self.poll_inflight = false;
                self.conn_error = None;
                // 消えたセンサーの履歴を捨てる
                self.temp_history.retain(|k, _| {
                    s.temps.iter().any(|t| format!("{}/{}", t.chip, t.label) == *k)
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
    let mut app = App::new(
        socket.to_path_buf(),
        cfg.curves,
        cfg.pwm_min,
        cfg.pwm_max,
        tx,
        rx,
    );
    if let Some(e) = cfg.error {
        app.notify(format!("config: {e}"));
    }

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
    error: Option<String>,
}

fn load_config(config_path: Option<&Path>) -> TuiConfig {
    let mut cfg = TuiConfig {
        curves: Vec::new(),
        pwm_min: DEFAULT_PWM_MIN,
        pwm_max: DEFAULT_PWM_MAX,
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
            cfg.pwm_min = config.control.min_pwm.max(DEFAULT_PWM_MIN);
            cfg.pwm_max = config.control.max_pwm.min(100).max(cfg.pwm_min);
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

    if let Some(pwm) = app.pwm_dialog {
        let step: i8 = if modifiers.contains(KeyModifiers::SHIFT) {
            5
        } else {
            1
        };
        match code {
            KeyCode::Left => {
                app.pwm_dialog = Some(clamp_pwm(pwm, -step, app.pwm_min, app.pwm_max))
            }
            KeyCode::Right => {
                app.pwm_dialog = Some(clamp_pwm(pwm, step, app.pwm_min, app.pwm_max))
            }
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
            'e' => app.notify("curve editor is not implemented yet (config: edit pmgfand.toml)"),
            'l' => app.notify("logs: see `journalctl -u pmgfand`"),
            _ => {}
        },
        _ => {}
    }
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
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (mode, state, pwm, uptime) = match &app.status {
        Some(s) => (
            mode_label(&s.mode),
            format!("{:?}", s.state),
            s.pwm.map(|p| format!("{p}%")).unwrap_or_else(|| "--".into()),
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
        Span::styled(mode, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("   State: ", Style::default().fg(Color::DarkGray)),
        Span::styled(state, Style::default().fg(state_color)),
        Span::styled("   PWM: ", Style::default().fg(Color::DarkGray)),
        Span::styled(pwm, Style::default().fg(Color::Yellow)),
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
                Span::styled(format!(" {:<12}", fan.name), Style::default().fg(Color::White)),
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
    let block = Block::default().title(" Temperatures ").borders(Borders::ALL);
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
                Span::styled(format!("{:<13}", t.label), Style::default().fg(Color::White)),
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
            tx,
            rx,
        );
        // seed が範囲外でも開いた時点でクランプされる
        app.status = Some(Status {
            state: DaemonState::Controlling,
            mode: Mode::FixedPwm(20),
            pwm: Some(20),
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
