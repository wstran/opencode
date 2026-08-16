//! Screenshot generator for the README. Renders through the same renderer the
//! terminal uses and writes the cell grid out as SVG, so the pictures are the
//! actual program rather than a mock-up and can be regenerated when the UI
//! changes. Test-only: nothing here is compiled into the binary.

#[cfg(test)]
mod shots {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    use crate::app::App;

    const CELL_W: f32 = 8.4;
    const CELL_H: f32 = 18.0;
    const FONT: f32 = 14.0;
    const PAD: f32 = 18.0;

    // The terminal background the shots are taken against.
    const BG: &str = "#1e2830";
    const DEFAULT_FG: &str = "#c0c5ce";

    fn hex(c: Color, fallback: &str) -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),

            _ => fallback.to_string(),
        }
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    fn to_svg(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        let width = PAD * 2.0 + w as f32 * CELL_W;

        let height = PAD * 2.0 + h as f32 * CELL_H;

        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
             viewBox=\"0 0 {width:.0} {height:.0}\" font-family=\"ui-monospace,SFMono-Regular,\
             'SF Mono',Menlo,Consolas,'Liberation Mono',monospace\" font-size=\"{FONT}\">\n\
             <rect width=\"100%\" height=\"100%\" rx=\"10\" fill=\"{BG}\"/>\n"
        );

        // Background runs first so text always paints on top of them.
        for y in 0..h {
            let mut x = 0;

            while x < w {
                let bg = buf.cell((x, y)).unwrap().bg;

                if matches!(bg, Color::Rgb(..)) {
                    let start = x;

                    while x < w && buf.cell((x, y)).unwrap().bg == bg {
                        x += 1;
                    }

                    let rx = PAD + start as f32 * CELL_W;

                    let ry = PAD + y as f32 * CELL_H;

                    let rw = (x - start) as f32 * CELL_W;

                    out.push_str(&format!(
                        "<rect x=\"{rx:.1}\" y=\"{ry:.1}\" width=\"{rw:.1}\" height=\"{CELL_H:.1}\" fill=\"{}\"/>\n",
                        hex(bg, BG)
                    ));
                } else {
                    x += 1;
                }
            }
        }

        // Then one <text> per run of identical styling, with textLength so the
        // grid stays aligned whatever monospace font the viewer has.
        for y in 0..h {
            let mut x = 0;

            while x < w {
                let cell = buf.cell((x, y)).unwrap();

                let (fg, modifier) = (cell.fg, cell.modifier);

                let start = x;

                let mut text = String::new();

                while x < w {
                    let c = buf.cell((x, y)).unwrap();

                    if c.fg != fg || c.modifier != modifier {
                        break;
                    }

                    text.push_str(c.symbol());

                    x += 1;
                }

                if text.trim().is_empty() {
                    continue;
                }

                let tx = PAD + start as f32 * CELL_W;

                let ty = PAD + y as f32 * CELL_H + CELL_H * 0.75;

                let len = text.chars().count() as f32 * CELL_W;

                let mut style = String::new();

                if modifier.contains(Modifier::BOLD) {
                    style.push_str(" font-weight=\"600\"");
                }

                if modifier.contains(Modifier::ITALIC) {
                    style.push_str(" font-style=\"italic\"");
                }

                out.push_str(&format!(
                    "<text x=\"{tx:.1}\" y=\"{ty:.1}\" fill=\"{}\" textLength=\"{len:.1}\" \
                     lengthAdjust=\"spacingAndGlyphs\" xml:space=\"preserve\"{style}>{}</text>\n",
                    hex(fg, DEFAULT_FG),
                    esc(&text)
                ));
            }
        }

        out.push_str("</svg>\n");

        out
    }

    fn shot(app: &mut App, w: u16, h: u16, name: &str) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();

        terminal.draw(|f| crate::ui::render(f, app)).unwrap();

        let svg = to_svg(terminal.backend().buffer(), w, h);

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join(name);

        std::fs::write(&path, svg).unwrap();

        println!("wrote {}", path.display());
    }

    /// Copy the real sources into a scratch tree, minus this generator, and run
    /// from inside it. That keeps a temporary file out of the pictures and lets
    /// the status bar show a short relative path instead of a home directory.
    fn staged_project() -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let stage = std::env::temp_dir().join("ocode-shots").join("ocode");

        let _ = std::fs::remove_dir_all(stage.parent().unwrap());

        std::fs::create_dir_all(stage.join("src")).unwrap();

        for entry in std::fs::read_dir(root.join("src")).unwrap() {
            let path = entry.unwrap().path();

            let name = path.file_name().unwrap().to_string_lossy().to_string();

            if name == "shot.rs" {
                continue;
            }

            std::fs::copy(&path, stage.join("src").join(&name)).unwrap();
        }

        for f in ["Cargo.toml", "README.md", "LICENSE"] {
            let _ = std::fs::copy(root.join(f), stage.join(f));
        }

        std::fs::create_dir_all(stage.join("assets")).unwrap();

        stage
    }

    fn select_in_tree(app: &mut App, name: &str) {
        if let Some(i) = app.tree.nodes.iter().position(|n| n.name == name) {
            app.tree.selected = i;
        }
    }


    /// Regenerate the README pictures. Step one writes `docs/*.svg`:
    ///
    /// ```sh
    /// cargo test --bin ocode -- --ignored write_readme_screenshots
    /// ```
    ///
    /// Step two rasterises them, since a PNG is what renders everywhere.
    /// Quick Look does the render and pads to a square, so it is cropped back:
    ///
    /// ```sh
    /// cd docs && mkdir -p /tmp/ql && for n in welcome editor find inspect; do
    ///   W=$(sed -n 's/.*width="\([0-9]*\)".*/\1/p' $n.svg | head -1)
    ///   H=$(sed -n 's/.*height="\([0-9]*\)".*/\1/p' $n.svg | head -1)
    ///   qlmanage -t -s $((W*2)) -o /tmp/ql "$n.svg" >/dev/null 2>&1
    ///   python3 -c "from PIL import Image; \
    ///     Image.open('/tmp/ql/$n.svg.png').crop((0,0,$W*2,$H*2)).save('$n.png')"
    /// done && rm -f *.svg && rm -rf /tmp/ql
    /// ```
    ///
    /// Ignored by default because it writes into `docs/`, which a plain test
    /// run has no business doing.
    #[test]
    #[ignore = "writes docs/, run on purpose when the UI changes"]
    fn write_readme_screenshots() {
        let stage = staged_project();

        let previous = std::env::current_dir().unwrap();

        std::env::set_current_dir(&stage).unwrap();

        // Welcome screen.
        let mut app = App::new(std::path::PathBuf::from("."), false).unwrap();

        app.picker = None;

        shot(&mut app, 92, 16, "welcome.svg");

        // Editor with the file tree open, showing the project's own source.
        let mut app = App::new(std::path::PathBuf::from("src/buffer.rs"), false).unwrap();

        app.picker = None;

        app.tree_visible = true;

        app.tree.refresh();

        select_in_tree(&mut app, "buffer.rs");

        for _ in 0..7 {
            app.buffer.as_mut().unwrap().move_down();
        }

        shot(&mut app, 108, 30, "editor.svg");

        // Find: every occurrence tinted, the current one selected. Step forward
        // a few times to land where several matches share the screen.
        let mut app = App::new(std::path::PathBuf::from("src/buffer.rs"), false).unwrap();

        app.picker = None;

        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for c in "cursor_col".chars() {
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)));
        }

        for _ in 0..9 {
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));
        }

        // Park the view so the current match sits mid-screen with its
        // neighbours, rather than on the last row where a jump leaves it.
        {
            let buf = app.buffer.as_mut().unwrap();

            buf.scroll_row = buf.cursor_line.saturating_sub(11);
        }

        app.scroll_free = true;

        shot(&mut app, 108, 24, "find.svg");

        // The inspector: what a file says about itself, then its bytes. Built
        // here rather than pointing at a file that exists on one machine, so
        // the picture is reproducible.
        let mp3 = stage.join("song.mp3");

        let mut bytes = Vec::new();

        let frames: Vec<(&[u8; 4], &str)> = vec![
            (b"TIT2", "Blue Monday"),
            (b"TPE1", "New Order"),
            (b"TALB", "Power, Corruption & Lies"),
            (b"TYER", "1983"),
            (b"TCON", "Synth-pop"),
        ];

        let mut body = Vec::new();

        for (id, value) in &frames {
            body.extend_from_slice(*id);

            // Frame size counts the encoding byte with the text.
            body.extend_from_slice(&((value.len() + 1) as u32).to_be_bytes());

            body.extend_from_slice(&[0, 0, 0]);

            body.extend_from_slice(value.as_bytes());
        }

        bytes.extend_from_slice(b"ID3\x04\x00\x00");

        // The tag size is synchsafe: seven bits per byte.
        let size = body.len() as u32;

        bytes.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);

        bytes.extend_from_slice(&body);

        bytes.extend((0..2048u32).map(|i| (i.wrapping_mul(37) % 251) as u8));

        std::fs::write(&mp3, &bytes).unwrap();

        let mut app = App::new(std::path::PathBuf::from("song.mp3"), false).unwrap();

        app.picker = None;

        shot(&mut app, 108, 22, "inspect.svg");

        std::env::set_current_dir(previous).unwrap();

        let _ = std::fs::remove_dir_all(stage.parent().unwrap());
    }
}
