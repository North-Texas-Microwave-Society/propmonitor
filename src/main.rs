mod config;
mod error;
mod measure;
mod timefmt;
mod ui;
mod worker;
mod yaml;

use std::io::{self, Stdout};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::Config;
use crate::error::{Context, Result};
use crate::ui::{render, App};
use crate::worker::{run_worker, WorkerEvent};

/// RAII guard: enable raw mode + alternate screen on construct, restore on
/// drop. Also installs a panic hook so a panic in either thread doesn't
/// leave the terminal wedged.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        // Install the panic hook BEFORE entering raw mode so a panic during
        // enable_raw_mode is still cleaned up. The hook chains to the
        // previous hook so the panic message still surfaces.
        let prev_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            prev_hook(info);
        }));

        enable_raw_mode().context("enable_raw_mode failed")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("EnterAlternateScreen failed")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("Terminal::new failed")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen).ok();
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.get(1).map(String::as_str).unwrap_or("config.yaml");
    let cfg = Config::load(config_path)
        .with_context(|| format!("failed to load config from {}", config_path))?;

    // Set up the terminal. The Drop on this guard restores the terminal
    // before main returns, even on Err or panic.
    let mut guard = TerminalGuard::enter()?;

    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_cfg = cfg.clone();
    let worker_stop = stop.clone();
    let worker_handle = thread::spawn(move || run_worker(worker_cfg, tx, worker_stop));

    let mut app = App::new(cfg);

    // Render at ~30 Hz so the live activity bar feels responsive even if
    // the worker only ticks 10 times a second.
    let tick = Duration::from_millis(33);
    let result = run_event_loop(&mut guard.terminal, &mut app, &rx, &stop, tick);

    // Tell the worker to stop and join it. The worker reports its own
    // errors via the channel, so we don't care about its return value.
    stop.store(true, Ordering::Relaxed);
    let _ = worker_handle.join();

    // Restore the terminal explicitly before printing any error so it
    // lands on the user's real shell rather than the alternate screen.
    drop(guard);

    if let Some(err) = app.error {
        eprintln!("propmonitor: {}", err);
    }

    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mpsc::Receiver<WorkerEvent>,
    stop: &Arc<AtomicBool>,
    tick: Duration,
) -> Result<()> {
    loop {
        // Drain any worker events that have arrived.
        loop {
            match rx.try_recv() {
                Ok(WorkerEvent::WindowStarted { at }) => app.on_window_started(at),
                Ok(WorkerEvent::FrameTick { in_band_dbfs }) => app.on_frame_tick(in_band_dbfs),
                Ok(WorkerEvent::WindowComplete(m)) => app.on_window_complete(m),
                Ok(WorkerEvent::Q65Decodes(rows)) => app.on_q65_decodes(rows),
                Ok(WorkerEvent::Error(e)) => {
                    app.error = Some(e);
                    app.should_quit = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.should_quit = true;
                    break;
                }
            }
        }

        terminal.draw(|f| render(f, app))?;

        if app.should_quit {
            stop.store(true, Ordering::Relaxed);
            return Ok(());
        }

        // Poll keyboard with the render-tick budget so the loop wakes up at
        // ~30 Hz even when nothing is happening.
        if event::poll(tick).context("event::poll failed")? {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read().context("event::read failed")?
            {
                let quit = matches!(code, KeyCode::Char('q') | KeyCode::Esc)
                    || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    stop.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
    }
}
