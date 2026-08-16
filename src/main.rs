mod app;
mod buffer;
mod comment;
mod config;
mod highlight;
mod media;
mod meta;
mod shot;
mod tree;
mod ui;

use std::io::{self, Stdout, Write};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyEventKind, MouseEventKind};
use crossterm::{execute, queue};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Which image is on screen and the cell box it occupies, as the run loop
/// tracks it between frames.
type Painted = Option<(u64, (u16, u16, u16, u16))>;

// Mouse reporting: 1000 (press/release) + 1002 (motion only while a button is
// held, which is what makes drag-select work) + 1006 (SGR coordinates, so
// columns past 223 still report). Deliberately not 1003 (any-motion): that
// reports every pointer move and would wake the render loop while idle.
//
// The trailing `\x1b[>1s` is XTSHIFTESCAPE: it asks the terminal to send
// shift+click to us instead of using shift for its own selection, so Shift+click
// can extend the editor selection. It is scoped to this program and reset on
// exit, so the terminal's shift-selection is untouched everywhere else.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[>1s";
const MOUSE_OFF: &[u8] = b"\x1b[>0s\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// ocode — a fast terminal code reader & editor.
#[derive(Parser)]
#[command(name = "ocode", version, about)]
struct Cli {
    /// File or directory to open (defaults to the current directory).
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Re-open the style picker to choose and save a different color scheme.
    #[arg(short = 's', long = "style")]
    style: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if !cli.path.exists() {
        anyhow::bail!("path does not exist: {}", cli.path.display());
    }

    let mut app = App::new(cli.path, cli.style)?;

    let mut terminal = setup_terminal()?;

    let result = run(&mut terminal, &mut app);

    restore_terminal(&mut terminal)?;

    result
}

fn run(terminal: &mut Tui, app: &mut App) -> Result<()> {
    // The cell box of the kitty image currently painted on screen (images are a
    // separate layer from ratatui's cell buffer, so the loop manages them).
    let mut shown: Painted = None;

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        if app.should_quit {
            if shown.is_some() {
                clear_image()?;
            }

            return Ok(());
        }

        sync_image(app, &mut shown)?;

        // Block on input, but wake periodically while a file is open (to watch
        // for external changes) or while a flash message is fading.
        match app.wake_after() {
            Some(timeout) => {
                if event::poll(timeout)? {
                    drain_input(app)?;
                } else {
                    app.tick();
                }
            }

            None => drain_input(app)?,
        }
    }
}

/// Paint or remove the kitty image to match what the renderer asked for. Only
/// acts on a change, so a static image is transmitted once and left alone.
fn sync_image(app: &App, shown: &mut Painted) -> Result<()> {
    // Keyed by image as well as by box: two images opened one after the other
    // land on the same box, and comparing the box alone would skip the repaint
    // and leave the first one on screen.
    let want = app.image_placement();

    if want == *shown {
        return Ok(());
    }

    let mut out = io::stdout();

    out.write_all(media::kitty_delete())?;

    if let Some((_, (x, y, cols, rows))) = want {
        queue!(out, MoveTo(x, y))?;

        out.write_all(&app.kitty_image_sequence(cols, rows))?;
    }

    out.flush()?;

    *shown = want;

    Ok(())
}

fn clear_image() -> Result<()> {
    let mut out = io::stdout();

    out.write_all(media::kitty_delete())?;

    out.flush()?;

    Ok(())
}

/// Handle one event. Returns `true` when it may have moved the panes around, so
/// the caller redraws before mapping any further mouse position against them.
fn read_event(app: &mut App) -> Result<bool> {
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            app.on_key(key);

            Ok(false)
        }

        Event::Mouse(mouse) => {
            let opens_something = matches!(mouse.kind, MouseEventKind::Down(_));

            app.on_mouse(mouse);

            Ok(opens_something)
        }

        _ => Ok(false),
    }
}

/// Drain everything already queued before returning to the draw. One wheel or
/// drag gesture arrives as a burst of events; redrawing between each is what
/// makes scrolling feel like it is lagging behind the pointer.
fn drain_input(app: &mut App) -> Result<()> {
    if read_event(app)? {
        return Ok(());
    }

    // Bounded so a device that produces events faster than we consume them can
    // never starve the redraw.
    for _ in 0..512 {
        if app.should_quit || !event::poll(Duration::ZERO)? {
            break;
        }

        if read_event(app)? {
            break;
        }
    }

    Ok(())
}

fn setup_terminal() -> Result<Tui> {
    install_panic_hook();

    enable_raw_mode().context("enabling raw mode")?;

    let mut stdout = io::stdout();

    execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;

    stdout.write_all(MOUSE_ON).context("enabling mouse reporting")?;

    stdout.flush().context("enabling mouse reporting")?;

    let backend = CrosstermBackend::new(stdout);

    Terminal::new(backend).context("creating terminal")
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().context("disabling raw mode")?;

    terminal
        .backend_mut()
        .write_all(MOUSE_OFF)
        .context("disabling mouse reporting")?;

    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leaving alternate screen")?;

    terminal.show_cursor().context("showing cursor")?;

    Ok(())
}

/// Restore the terminal on panic so a crash never leaves the user in a broken
/// raw-mode screen.
fn install_panic_hook() {
    let hook = panic::take_hook();

    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();

        // Leaving mouse reporting on would spray escape codes into the user's
        // shell on every click after the crash.
        let _ = io::stdout().write_all(MOUSE_OFF);

        let _ = execute!(io::stdout(), LeaveAlternateScreen);

        hook(info);
    }));
}
