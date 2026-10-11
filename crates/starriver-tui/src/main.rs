use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use starriver_tui::app::AppState;
use starriver_tui::host::Host;
use starriver_tui::ui;

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let fullscreen = args.iter().any(|a| a == "--fullscreen" || a == "-f");
    let config_path = parse_args(&args).map(|p| p.to_string_lossy().into_owned());

    let host = Host::new(config_path)?;
    // No host-side "current agent": the TUI owns its choice and names the
    // coding agent on every execute (persona installs per run).
    let mut app = AppState::new(Box::new(host))?;

    // WI-80: inline scrollback is the default; --fullscreen keeps the
    // classic whole-screen alternate-screen experience.
    let result = if fullscreen {
        let mut terminal = setup_terminal()?;
        let r = run_loop(&mut terminal, &mut app);
        restore_terminal(&mut terminal)?;
        r
    } else {
        let mut out = std::io::stdout();
        let r = run_loop_inline(&mut out, &mut app);
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        );
        r
    };
    app.shutdown();
    result
}

fn parse_args(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--fullscreen" || arg == "-f" {
            continue;
        }
        if arg == "--config" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen, crossterm::event::EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// WI-80 inline loop: no alternate screen. Finished turns print into the
/// terminal's native scrollback; each frame redraws only the live tail.
fn run_loop_inline(out: &mut Stdout, app: &mut AppState) -> Result<()> {
    use starriver_tui::inline::{render_live, InlineRenderer};

    crossterm::execute!(
        out,
        crossterm::event::EnableBracketedPaste,
        crossterm::cursor::Hide
    )?;
    enable_raw_mode()?;
    let mut renderer = InlineRenderer::new();
    while !app.should_quit {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        // Flush finished transcript rows into the scrollback first.
        let committed = app.take_committed(8);
        let live_rows = app.visible_transcript(false).len();
        let live_h = InlineRenderer::live_height(app, h, live_rows);
        let buf = render_live(app, w, live_h);
        renderer.frame(out, &buf, &committed)?;
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key)?,
                Event::Paste(text) => app.paste_text(&text),
                _ => {}
            }
        }
        app.tick()?;
    }
    renderer.finish(out)?;
    disable_raw_mode()?;
    Ok(())
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut AppState) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key)?,
                // WI-78: bracketed paste — multi-line clipboard text lands
                // in the composer as a whole (normalized), never auto-send.
                Event::Paste(text) => app.paste_text(&text),
                _ => {}
            }
        }
        app.tick()?;
    }
    Ok(())
}
