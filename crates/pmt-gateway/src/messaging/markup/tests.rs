//! Golden conversions (spec: platform-adapters, Markup conversion per
//! platform).

use super::*;

fn descriptor(platform: &str, markup: Markup) -> Descriptor {
    Descriptor {
        markup,
        ..Descriptor::new(platform, 4096)
    }
}

const TABLE: &str = "Status:\n\n| Service | State | Owner |\n|---|---|---|\n| api | up | ada |\n| db | degraded | bob |\n| web | up | cy |\n";

#[test]
fn a_small_table_on_telegram_is_bullet_groups_without_pipes() {
    let out = render(&descriptor("telegram", Markup::Platform), TABLE, false);
    assert_eq!(
        out,
        "Status:\n\n\
         • *Service:* api\n  *State:* up\n  *Owner:* ada\n\
         • *Service:* db\n  *State:* degraded\n  *Owner:* bob\n\
         • *Service:* web\n  *State:* up\n  *Owner:* cy\n"
    );
    assert!(!out.contains('|'));
}

#[test]
fn a_large_table_on_telegram_is_a_fenced_block() {
    let mut table = String::from("| a | b | c | d | e |\n|---|---|---|---|---|\n");
    table.push_str("| 1 | 2 | 3 | 4 | 5 |\n");
    let out = render(&descriptor("telegram", Markup::Platform), &table, false);
    assert!(out.starts_with("```\n| a | b | c | d | e |"), "{out}");
    assert!(out.contains("\n```"), "{out}");
}

#[test]
fn telegram_markdown_v2_escapes_outside_code_only() {
    let text = "**Done** in 1.5s (see [docs](https://x.y/a_(b))). Use `a_b*c` or:\n```rust\nlet x = a.b_c(1);\n```";
    let out = render(&descriptor("telegram", Markup::Platform), text, false);
    assert_eq!(
        out,
        "*Done* in 1\\.5s \\(see [docs](https://x.y/a_(b\\))\\)\\. Use `a_b*c` or:\n```rust\nlet x = a.b_c(1);\n```"
    );
}

#[test]
fn a_link_on_a_plain_platform_shows_its_url() {
    let out = render(
        &descriptor("sms", Markup::Plain),
        "Read [docs](https://x.y) now",
        false,
    );
    assert_eq!(out, "Read docs (https://x.y) now");
}

#[test]
fn whatsapp_style_conversion() {
    let text = "# Plan\n**bold** ~~gone~~ *it* [t](https://u.v)\n```\n**kept**\n```";
    let out = render(&descriptor("whatsapp", Markup::Platform), text, false);
    assert_eq!(
        out,
        "*Plan*\n*bold* ~gone~ _it_ t (https://u.v)\n```\n**kept**\n```"
    );
}

#[test]
fn slack_mrkdwn_conversion() {
    let text = "**a** & <b> [site](https://s.t) `x<y`";
    let out = render(&descriptor("slack", Markup::Platform), text, false);
    assert_eq!(out, "*a* &amp; &lt;b&gt; <https://s.t|site> `x&lt;y`");
}

#[test]
fn matrix_html_conversion() {
    let text = "## Title\nSee **this** & [that](https://x.y)\n```py\nif a < b: pass\n```";
    let out = render(&descriptor("matrix", Markup::Html), text, false);
    assert_eq!(
        out,
        "<h2>Title</h2><br>See <b>this</b> &amp; <a href=\"https://x.y\">that</a><br><pre><code class=\"language-py\">if a &lt; b: pass</code></pre>"
    );
    assert_eq!(html_to_text(&out), "Title\nSee this & that\nif a < b: pass");
}

#[test]
fn markdown_platforms_get_the_text_as_written_and_plain_resends_drop_markup() {
    let text = "**b** [l](https://u)";
    assert_eq!(
        render(&descriptor("discord", Markup::Markdown), text, false),
        text
    );
    assert_eq!(
        render(&descriptor("telegram", Markup::Platform), text, true),
        "**b** l (https://u)"
    );
}

#[test]
fn signal_styles_are_utf16_ranges_over_plain_text() {
    let (text, styles) =
        signal_styles("Hi 👋 **bold** _it_ `code` ~~no~~ ||secret|| [docs](https://x.y)");
    assert_eq!(text, "Hi 👋 bold it code no secret docs (https://x.y)");
    let specs: Vec<String> = styles.iter().map(Style::spec).collect();
    assert_eq!(
        specs,
        vec![
            "6:4:BOLD",
            "11:2:ITALIC",
            "14:4:MONOSPACE",
            "19:2:STRIKETHROUGH",
            "22:6:SPOILER"
        ]
    );
}

#[test]
fn snake_case_words_are_not_italics() {
    let out = render(
        &descriptor("whatsapp", Markup::Platform),
        "set max_message_length and _this_",
        false,
    );
    assert_eq!(out, "set max_message_length and _this_");
}
