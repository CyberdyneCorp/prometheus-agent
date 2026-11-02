//! Screen snapshots (spec: tui, Status line and accessibility; Slash
//! command flow; Configurable key bindings; Composer and path completion):
//! the screen rendered into a test backend and compared with the text in
//! `tests/snapshots/<name>.txt`. `PMT_UPDATE_SNAPSHOTS=1` rewrites them
//! (review the diff before committing).

use pmt_config::schema::TuiTheme;
use pmt_tui::app::{App, Card};
use pmt_tui::keys::Bindings;
use pmt_tui::status::Status;
use pmt_tui::theme::Theme;
use pmt_tui::transcript::Kind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn screen(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..height {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buffer[(x, y)].symbol());
        }
        text.push_str(row.trim_end());
        text.push('\n');
    }
    text
}

fn check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("PMT_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (PMT_UPDATE_SNAPSHOTS=1 writes it)", path.display()));
    assert_eq!(actual, expected, "snapshot {name} differs");
}

fn app() -> App {
    let status = Status {
        profile: "default".into(),
        model: Some("mock/m1".into()),
        context_tokens: Some(2048),
        context_window: Some(8192),
        cost_usd: Some(0.0042),
        ..Status::default()
    };
    let mut app = App::new(Theme::new(TuiTheme::Dark, true), status);
    app.transcript.push(Kind::User, "summarise the build log");
    app.transcript.push(Kind::Tool, "read_file");
    app.transcript.push(
        Kind::Assistant,
        "The build failed in the linker step: two crates define the same symbol.",
    );
    app
}

#[test]
fn the_screen_shows_transcript_composer_and_status() {
    let mut app = app();
    app.composer.set("and how do I fix it?");
    check("conversation-80x14", &screen(&app, 80, 14));
}

#[test]
fn a_40_by_12_terminal_keeps_the_composer_and_the_last_line() {
    let mut app = app();
    for n in 0..30 {
        app.transcript.push(Kind::Notice, format!("notice {n}"));
    }
    app.transcript
        .push(Kind::Assistant, "the newest line of the transcript");
    app.composer
        .set("a prompt long enough to scroll past forty columns");
    let shown = screen(&app, 40, 12);
    assert!(shown.contains("the newest line of the"), "{shown}");
    assert!(shown.contains("› "), "{shown}");
    check("narrow-40x12", &shown);
    // Smaller still: nothing panics.
    for (w, h) in [(10, 3), (1, 1), (40, 4)] {
        screen(&app, w, h);
    }
}

#[test]
fn an_approval_card_sits_above_the_composer_and_counts_in_the_status() {
    let mut app = app();
    app.status.approvals = 1;
    app.status.busy = true;
    app.status.reduced_motion = true;
    app.card = Some(Card {
        title: "approval needed: terminal rm -rf build".into(),
        body: "dangerous-pattern: recursive delete (rm -rf)".into(),
        hint: "[o]nce  [s]ession  [a]lways  [d]eny   (answer within 300 s)".into(),
    });
    check("approval-80x14", &screen(&app, 80, 14));
}

#[test]
fn slash_completions_list_above_the_composer() {
    let mut app = app();
    app.composer.set("/comp");
    app.completions = vec![
        "/compact  compact the context now".into(),
        "/compress  summarise older turns".into(),
    ];
    check("completions-60x12", &screen(&app, 60, 12));
}

#[test]
fn keys_lists_the_effective_bindings() {
    let mut app = app();
    app.transcript
        .push(Kind::Notice, Bindings::default().describe());
    check("keys-60x24", &screen(&app, 60, 24));
}

/// Scenario: Hiding the status bar (the screen side).
#[test]
fn a_hidden_status_bar_gives_its_row_to_the_transcript() {
    let mut app = app();
    app.status_visible = false;
    let shown = screen(&app, 60, 10);
    assert!(!shown.contains("mock/m1"), "{shown}");
    check("no-status-60x10", &shown);
}

/// Scenario: Battery on a desktop tower (a battery leads when there is
/// one) and Switching style (the busy indicator).
#[test]
fn the_battery_leads_the_status_and_the_busy_glyph_follows_the_style() {
    let mut app = app();
    app.status.battery = Some(pmt_core::battery::Battery {
        percent: 64,
        charging: true,
    });
    app.status.busy = true;
    app.status.indicator = pmt_config::schema::BusyIndicator::Ascii;
    app.status.busy_for = pmt_tui::busy::PERIOD * 2;
    check("battery-busy-70x8", &screen(&app, 70, 8));
}

/// Scenario: Insert, not run (what the palette shows).
#[test]
fn the_palette_lists_matching_commands_with_their_disposition() {
    use pmt_tui::app::Overlay;
    use pmt_tui::palette::{Entry, Palette};
    let entry = |name: &str, description: &str, disposition: &str| Entry {
        name: name.into(),
        description: description.into(),
        disposition: disposition.into(),
    };
    let mut palette = Palette::new(vec![
        entry("compress", "summarise older turns", "exec"),
        entry("compact", "compact the context now", "exec"),
        entry("deploy", "deploy the service", "skill"),
        entry("clear", "clear the screen", "client"),
    ]);
    palette.push('c');
    palette.push('o');
    let mut app = app();
    app.overlay = Some(Overlay::Palette(palette));
    check("palette-70x14", &screen(&app, 70, 14));
}

/// Scenario: Dispatching a second session (what the switcher shows).
#[test]
fn the_switcher_lists_live_sessions_and_new() {
    use pmt_tui::app::Overlay;
    use pmt_tui::switcher::{Row, Switcher};
    let mut switcher = Switcher::new(vec![
        Row {
            id: "ses_01A".into(),
            title: Some("build log".into()),
            busy: true,
            shown: true,
        },
        Row {
            id: "ses_01B".into(),
            title: None,
            busy: false,
            shown: false,
        },
    ]);
    switcher.move_selection(2);
    for c in "write the tests".chars() {
        switcher.type_char(c);
    }
    let mut app = app();
    app.status.live = 2;
    app.overlay = Some(Overlay::Switcher(switcher));
    check("switcher-90x14", &screen(&app, 90, 14));
}

/// Scenario: Many skills installed. Scenario: Unsupported macro.
#[test]
fn the_banner_heads_the_transcript_and_answers_render_math() {
    use pmt_tui::banner::{Banner, Contents};
    let contents = Contents {
        tools: vec!["read_file".into(), "terminal".into(), "patch".into()],
        skills: (0..80).map(|n| format!("skill-{n}")).collect(),
        system_prompt: vec!["You are Prometheus.".into()],
        mcp_servers: Vec::new(),
    };
    let mut app = App::new(Theme::new(TuiTheme::Dark, true), Status::default());
    app.banner = Some(Banner::new(
        "Prometheus 0.1.0 · default · mock/m1".into(),
        contents,
    ));
    app.transcript.push(
        Kind::Assistant,
        r"Energy is $E = mc^2$, a half is $\frac{1}{2}$, it costs $5 and $10; $\begin{tikzpicture}$ is not supported.",
    );
    let shown = screen(&app, 70, 14);
    assert!(shown.contains("▸ Skills (80)"), "{shown}");
    assert!(!shown.contains("skill-1"), "{shown}");
    assert!(shown.contains(r"$\begin{tikzpicture}$"), "{shown}");
    check("banner-math-70x14", &shown);
}

/// Scenario: Long log paste (what the composer shows).
#[test]
fn a_long_paste_shows_as_one_chip_and_an_image_as_a_badge() {
    use pmt_tui::composer::Collapse;
    let mut app = app();
    let log = (1..=300)
        .map(|n| format!("log line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.composer.paste(
        &log,
        Collapse {
            lines: 5,
            chars: 2000,
        },
    );
    app.composer
        .attach_image(std::path::PathBuf::from("shot.png"));
    app.composer.insert_str("what failed?");
    check("chips-70x10", &screen(&app, 70, 10));
}
