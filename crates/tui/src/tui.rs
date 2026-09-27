//! Ratatui TUI。pmgfand の状態監視・モード切替を行う。
//! TUI を閉じてもデーモン側の制御は継続する（ビューアであり、
//! 制御本体ではない）。

use std::collections::{HashMap, VecDeque};
use std::io::stdout;
use std::path::{Path, PathBuf};
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

use crate::client;

/// 状態ポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// キー入力待ちの上限（これにより描画が周期的に回る）。
const TICK: Duration = Duration::from_millis(100);
/// 温度スパークラインの保持サンプル数。
const HISTORY_LEN: usize = 180;
/// notice の表示時間。
const NOTICE_TTL: Duration = Duration::from_secs(4);
/// Fixed PWM ダイアログの下限（daemon 側既定と同じ）。
const DIALOG_MIN_PWM: u8 = 30;

/// 最後に取得したデーモン状態。
#[derive(Debug, Clone)]
struct Status {
    state: DaemonState,
    mode: Mode,
    pwm: Option<u8>,
    fans: Vec<pmgfan_core::fan::FanReading>,
    temps: Vec<pmgfan_core::sensor::TempReading>,
    uptime_secs: f64,
}

struct App {
    socket: PathBuf,
    curves: Vec<Curve>,
    status: Option<Status>,
    conn_error: Option<String>,
    /// `chip/label` → 温度履歴
    temp_history: HashMap<String, VecDeque<u64>>,
    pwm_dialog: Option<u8>,
    notice: Option<(Instant, String)>,
    quit: bool,
}

impl App {
    fn new(socket: PathBuf, curves: Vec<Curve>) -> Self {
        Self {
            socket,
            curves,
            status: None,
            conn_error: None,
            temp_history: HashMap::new(),
            pwm_dialog: None,
            notice: None,
            quit: false,
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((Instant::now(), msg.into()));
    }

    async fn refresh(&mut self) {
        match client::request(&self.socket, &Request::GetStatus).await {
            Ok(Response::Status {
                state,
                mode,
                pwm,
                fans,
                temperatures,
                uptime_secs,
            }) => {
                self.conn_error = None;
                for t in &temperatures {
                    let key = format!("{}/{}", t.chip, t.label);
                    let hist = self.temp_history.entry(key).or_default();
                    hist.push_back((t.celsius * 10.0) as u64);
                    while hist.len() > HISTORY_LEN {
                        hist.pop_front();
                    }
                }
                self.status = Some(Status {
                    state,
                    mode,
                    pwm,
                    fans,
                    temps: temperatures,
                    uptime_secs,
                });
            }
            Ok(_) => self.conn_error = Some("unexpected response".into()),
            Err(e) => self.conn_error = Some(e.to_string()),
        }
    }

    async fn set_mode(&mut self, mode: Mode, label: &str) {
        match client::request(&self.socket, &Request::SetMode { mode }).await {
            Ok(resp) => match client::expect_ok(resp) {
                Ok(()) => self.notify(format!("mode applied: {label}")),
                Err(e) => self.notify(format!("{e:#}")),
            },
            Err(e) => self.notify(format!("{e:#}")),
        }
        self.refresh().await;
    }
}

/// TUI を実行する。戻るときは端末を元の状態に復元する。
pub async fn run(socket: &Path, config_path: Option<&Path>) -> Result<()> {
    let curves = load_curves(config_path);

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    let _guard = TerminalGuard;

    let mut app = App::new(socket.to_path_buf(), curves);
    app.refresh().await;
    let mut last_poll = Instant::now();

    while !app.quit {
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key.code, key.modifiers).await;
                }
            }
        }
        if last_poll.elapsed() >= POLL_INTERVAL {
            app.refresh().await;
            last_poll = Instant::now();
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

/// 設定ファイルから `[[curve]]` を読み込む（表示専用。
/// 読めない場合はカーブパネルを出さない）。
fn load_curves(config_path: Option<&Path>) -> Vec<Curve> {
    let Some(path) = config_path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(config) = toml::from_str::<Config>(&text) else {
        return Vec::new();
    };
    config
        .curves
        .iter()
        .filter_map(|c| Curve::new(c.sensor.clone(), c.points.clone()).ok())
        .collect()
}

async fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if let Some(pwm) = app.pwm_dialog {
        match code {
            KeyCode::Left => {
                let step = if modifiers.contains(KeyModifiers::SHIFT) {
                    5
                } else {
                    1
                };
                app.pwm_dialog = Some(pwm.saturating_sub(step).max(DIALOG_MIN_PWM));
            }
            KeyCode::Right => {
                let step = if modifiers.contains(KeyModifiers::SHIFT) {
                    5
                } else {
                    1
                };
                app.pwm_dialog = Some(pwm.saturating_add(step).min(100));
            }
            KeyCode::Enter => {
                app.pwm_dialog = None;
                app.set_mode(Mode::FixedPwm(pwm), &format!("fixed PWM {pwm}%"))
                    .await;
            }
            KeyCode::Esc => app.pwm_dialog = None,
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('a') => app.set_mode(Mode::IrmcAuto, "iRMC Auto").await,
        KeyCode::Char('c') => app.set_mode(Mode::Curve, "Curve").await,
        KeyCode::Char('f') => {
            let seed = app
                .status
                .as_ref()
                .and_then(|s| match s.mode {
                    Mode::FixedPwm(p) => Some(p),
                    _ => s.pwm,
                })
                .unwrap_or(40);
            app.pwm_dialog = Some(seed.max(DIALOG_MIN_PWM));
        }
        KeyCode::Char('r') => {
            app.notify("target RPM mode is not implemented yet (phase 7)")
        }
        KeyCode::Char('e') => {
            app.notify("curve editor is not implemented yet (config: edit pmgfand.toml)")
        }
        KeyCode::Char('l') => app.notify("logs: see `journalctl -u pmgfand`"),
        _ => {}
    }
}

fn draw(f: &mut Frame, app: &App) {
    let has_curves = !app.curves.is_empty();
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(9), // fans
        Constraint::Min(8),    // temperatures
    ];
    if has_curves {
        constraints.push(Constraint::Length(10)); // curve
    }
    constraints.push(Constraint::Length(2)); // footer
    let chunks = Layout::vertical(constraints).split(f.area());

    draw_header(f, app, chunks[0]);
    draw_fans(f, app, chunks[1]);
    draw_temps(f, app, chunks[2]);
    let mut idx = 3;
    if has_curves {
        draw_curve(f, app, chunks[3]);
        idx = 4;
    }
    draw_footer(f, app, chunks[idx]);

    if let Some(pwm) = app.pwm_dialog {
        draw_pwm_dialog(f, pwm);
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

    let lines: Vec<Line> = status
        .fans
        .iter()
        .take(inner.height as usize)
        .map(|fan| {
            let bar_w = inner.width.saturating_sub(34).max(4) as usize;
            let (rpm_s, bar) = match fan.rpm {
                Some(rpm) => {
                    let filled = rpm as usize * bar_w / max_rpm;
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
    // 行数が多いので上下2列に分けず縦に列挙する。
    let rows = Layout::vertical(
        status
            .temps
            .iter()
            .map(|_| Constraint::Length(1))
            .collect::<Vec<_>>(),
    )
    .split(inner);
    for (i, t) in status.temps.iter().enumerate() {
        if i >= rows.len() {
            break;
        }
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
        .split(rows[i]);
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
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{:>5.1}°C", t.celsius),
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
}

fn draw_curve(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Fan Curves ").borders(Borders::ALL);
    let canvas = Canvas::default()
        .block(block)
        .x_bounds([20.0, 100.0])
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
                ctx.print(21.0, 100.0 - i as f64 * 6.0, curve.sensor.to_string());
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

fn draw_pwm_dialog(f: &mut Frame, pwm: u8) {
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
        Paragraph::new(format!("←/→ ±1%   Shift+←/→ ±5%   (min {DIALOG_MIN_PWM}%)"))
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
    if c >= 85.0 {
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
