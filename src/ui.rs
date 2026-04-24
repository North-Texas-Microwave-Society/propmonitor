use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Local};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::Frame;

use crate::config::{Config, Mode};
use crate::measure::Measurement;

/// One row in the analog-mode history table.
pub struct HistoryRow {
    pub at: DateTime<Local>,
    pub measurement: Measurement,
}

/// One row in the Q65 decode list.
#[derive(Debug, Clone)]
pub struct Q65Row {
    pub at: DateTime<Local>,
    pub snr_db: f32,
    pub dt_s: f32,
    pub freq_hz: f32,
    pub message: String,
}

pub struct App {
    pub cfg: Config,
    pub history: VecDeque<HistoryRow>,
    pub last: Option<Measurement>,
    pub window_start: Option<Instant>,
    pub last_frame_dbfs: Option<f64>,
    pub last_noise_dbfs: Option<f64>,
    pub q65_decodes: VecDeque<Q65Row>,
    pub error: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            history: VecDeque::new(),
            last: None,
            window_start: None,
            last_frame_dbfs: None,
            last_noise_dbfs: None,
            q65_decodes: VecDeque::new(),
            error: None,
            should_quit: false,
        }
    }

    pub fn on_window_started(&mut self, at: Instant) {
        self.window_start = Some(at);
        self.last_frame_dbfs = None;
    }

    pub fn on_frame_tick(&mut self, dbfs: f64) {
        self.last_frame_dbfs = Some(dbfs);
    }

    pub fn on_window_complete(&mut self, m: Measurement) {
        self.last_noise_dbfs = Some(m.noise_dbfs);
        self.history.push_front(HistoryRow {
            at: Local::now(),
            measurement: m,
        });
        self.last = Some(m);
        while self.history.len() > 256 {
            self.history.pop_back();
        }
    }

    pub fn on_q65_decodes(&mut self, rows: Vec<Q65Row>) {
        for r in rows {
            self.q65_decodes.push_front(r);
        }
        while self.q65_decodes.len() > 256 {
            self.q65_decodes.pop_back();
        }
    }

    fn instant_snr_db(&self) -> Option<f64> {
        match (self.last_frame_dbfs, self.last_noise_dbfs) {
            (Some(s), Some(n)) => Some(s - n),
            _ => None,
        }
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    let outer = Block::default()
        .title(" propmonitor ")
        .borders(Borders::ALL);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if app.cfg.mode == Mode::Q65 {
        render_q65(f, inner, app);
    } else {
        render_analog(f, inner, app);
    }

    if let Some(err) = &app.error {
        let p = Paragraph::new(Line::from(vec![
            Span::raw("error: ").red().bold(),
            Span::raw(err.clone()),
        ]));
        let err_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 2,
        };
        f.render_widget(p, err_area);
    }
}

fn render_analog(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(inner);

    render_header(f, chunks[0], app);
    render_current(f, chunks[1], app);
    render_history(f, chunks[2], app);
    render_live(f, chunks[3], app);
}

fn render_q65(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(inner);

    render_header(f, chunks[0], app);
    render_q65_list(f, chunks[1], app);
    render_q65_footer(f, chunks[2], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let mode_str = mode_label(app.cfg.mode);
    let gain_str = match app.cfg.gain {
        Some(g) => format!("gain {:.0} dB", g),
        None => "gain auto".into(),
    };
    let line = Line::from(vec![
        Span::raw(format!("{:.6} MHz", app.cfg.frequency / 1e6))
            .bold()
            .cyan(),
        Span::raw("  "),
        Span::raw(mode_str).bold().yellow(),
        Span::raw("  "),
        Span::raw(format!("sr {:.3} MS/s", app.cfg.sample_rate / 1e6)),
        Span::raw("  "),
        Span::raw(gain_str),
        Span::raw("    "),
        Span::raw("(q to quit)").dim(),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_current(f: &mut Frame, area: Rect, app: &App) {
    let labels = Line::from(vec![
        Span::styled(format!("{:>10}", "noise"), Style::default().add_modifier(Modifier::DIM)),
        Span::styled(format!("{:>12}", "sig peak"), Style::default().add_modifier(Modifier::DIM)),
        Span::styled(format!("{:>12}", "sig avg"), Style::default().add_modifier(Modifier::DIM)),
        Span::styled(format!("{:>12}", "snr peak"), Style::default().add_modifier(Modifier::DIM)),
        Span::styled(format!("{:>12}", "snr avg"), Style::default().add_modifier(Modifier::DIM)),
    ]);

    let values = match &app.last {
        Some(m) => Line::from(vec![
            Span::raw(format!("{:>10.2}", m.noise_dbfs)),
            Span::raw(format!("{:>12.2}", m.signal_peak_dbfs)).bold(),
            Span::raw(format!("{:>12.2}", m.signal_avg_dbfs)),
            Span::raw(format!("{:>12.2}", m.snr_peak_db)).bold().green(),
            Span::raw(format!("{:>12.2}", m.snr_avg_db)).green(),
        ]),
        None => Line::from(Span::raw("  (waiting for first window to complete…)").dim()),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    f.render_widget(Paragraph::new(labels), chunks[0]);
    f.render_widget(Paragraph::new(values), chunks[1]);
}

fn render_history(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        Cell::from("TIME"),
        Cell::from("NOISE"),
        Cell::from("SIG_PK"),
        Cell::from("SIG_AV"),
        Cell::from("SNR_PK"),
        Cell::from("SNR_AV"),
    ])
    .style(Style::default().add_modifier(Modifier::DIM));

    let visible_rows = (area.height as usize).saturating_sub(2);
    let rows: Vec<Row> = app
        .history
        .iter()
        .take(visible_rows)
        .map(|h| {
            let m = &h.measurement;
            Row::new(vec![
                Cell::from(h.at.format("%H:%M:%S").to_string()),
                Cell::from(format!("{:>7.2}", m.noise_dbfs)),
                Cell::from(format!("{:>7.2}", m.signal_peak_dbfs)),
                Cell::from(format!("{:>7.2}", m.signal_avg_dbfs)),
                Cell::from(format!("{:>7.2}", m.snr_peak_db)),
                Cell::from(format!("{:>7.2}", m.snr_avg_db)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title(" history ").borders(Borders::TOP));
    f.render_widget(table, area);
}

fn render_live(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(Line::from(Span::raw(" live").dim())),
        chunks[0],
    );

    let (snr_ratio, snr_label) = match app.instant_snr_db() {
        Some(snr) => {
            let clamped = snr.clamp(0.0, 40.0);
            (clamped / 40.0, format!("in-band SNR {:+.1} dB", snr))
        }
        None => (0.0, "in-band SNR — (waiting for noise reference)".to_string()),
    };
    let snr_color = if snr_ratio > 0.5 {
        Color::Green
    } else if snr_ratio > 0.2 {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let snr_gauge = Gauge::default()
        .gauge_style(Style::default().fg(snr_color))
        .ratio(snr_ratio)
        .label(snr_label);
    f.render_widget(snr_gauge, chunks[1]);

    let (win_ratio, win_label) = match app.window_start {
        Some(start) => {
            let elapsed = start.elapsed().as_secs_f64().min(60.0);
            (elapsed / 60.0, format!("window {:>2.0} / 60 s", elapsed))
        }
        None => (0.0, "window — / 60 s".to_string()),
    };
    let win_gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(win_ratio)
        .label(win_label);
    f.render_widget(win_gauge, chunks[2]);
}

fn render_q65_list(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        Cell::from("TIME"),
        Cell::from("SNR"),
        Cell::from("DT"),
        Cell::from("HZ"),
        Cell::from("MESSAGE"),
    ])
    .style(Style::default().add_modifier(Modifier::DIM));

    let visible = (area.height as usize).saturating_sub(2);
    let rows: Vec<Row> = app
        .q65_decodes
        .iter()
        .take(visible)
        .map(|r| {
            Row::new(vec![
                Cell::from(r.at.format("%H:%M:%S").to_string()),
                Cell::from(format!("{:>4.0}", r.snr_db)),
                Cell::from(format!("{:>5.1}", r.dt_s)),
                Cell::from(format!("{:>5.0}", r.freq_hz)),
                Cell::from(r.message.clone()),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Min(20),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title(" Q65-60C decodes ").borders(Borders::TOP));
    f.render_widget(table, area);
}

fn render_q65_footer(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::raw(" live").dim())),
        chunks[0],
    );
    let (ratio, label) = match app.window_start {
        Some(start) => {
            let elapsed = start.elapsed().as_secs_f64().min(60.0);
            (elapsed / 60.0, format!("period {:>2.0} / 60 s", elapsed))
        }
        None => (0.0, "period — / 60 s".into()),
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, chunks[1]);
}

fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Usb => "USB",
        Mode::Lsb => "LSB",
        Mode::Am => "AM",
        Mode::Nfm => "NFM",
        Mode::Wfm => "WFM",
        Mode::Cw => "CW",
        Mode::Q65 => "Q65-60C",
    }
}
