//! Media in and out (spec: platform-adapters, Inbound media limits and
//! caching; Outbound attachments and path safety; Voice input
//! transcription; Voice replies and streaming TTS) against the fake
//! adapter and scripted speech providers.

use std::sync::Arc;

use pmt_gateway::messaging::{Attachment, AudioOp, ChatType, Descriptor, Inbound, MediaKind};

use crate::support::{Did, FakeSpeech, Harness, Setup, config, eventually, open, said};

fn with_attachment(kind: MediaKind, location: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Dm, "1", "1", text);
    let mut attachment = Attachment::new(kind, location);
    attachment.duration_secs = Some(7);
    inbound.attachments.push(attachment);
    inbound
}

fn media_dir(h: &Harness) -> std::path::PathBuf {
    h.home
        .path()
        .join("profiles")
        .join("default")
        .join("cache")
        .join("media")
}

#[tokio::test]
async fn a_download_lying_about_its_length_is_aborted_and_nothing_is_cached() {
    let table = "allow_all = true\n\n[gateway.media]\nmax_inbound_bytes = 2097152";
    let h = Harness::start(Setup::new(&config("telegram", table))).await;
    // Declares 1 MB, streams 200 chunks of 64 KiB (12.5 MiB).
    h.adapter("telegram")
        .media
        .lock()
        .unwrap()
        .insert("file-1".into(), (Some(1_048_576), 200, 65_536));
    h.send(
        "telegram",
        with_attachment(MediaKind::Document, "file-1", "see this"),
    )
    .await;
    said(&h.adapter("telegram"), "Attachment too large (3 MB > 2 MB)").await;
    let cached: Vec<_> = std::fs::read_dir(media_dir(&h))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(cached.is_empty(), "{cached:?}");
    said(&h.adapter("telegram"), "echo: see this").await;
}

#[tokio::test]
async fn an_accepted_attachment_is_cached_by_content() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.adapter("telegram")
        .media
        .lock()
        .unwrap()
        .insert("photo-1".into(), (Some(4096), 4, 1024));
    let mut inbound = with_attachment(MediaKind::Document, "photo-1", "what is this");
    inbound.attachments[0].name = Some("report.PDF".into());
    h.send("telegram", inbound).await;
    eventually("a turn", || !h.agents.texts().is_empty()).await;
    let text = h.agents.texts().remove(0);
    let cached: Vec<_> = std::fs::read_dir(media_dir(&h))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(cached.len(), 1);
    let name = cached[0].file_name().to_string_lossy().into_owned();
    assert!(
        std::path::Path::new(&name).extension() == Some("pdf".as_ref()) && name.len() == 64 + 4,
        "{name}"
    );
    assert!(text.contains(&name), "{text}");
}

#[tokio::test]
async fn a_failed_transcription_hands_the_agent_the_voice_note() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let speech = Arc::new(FakeSpeech::default());
    *speech.transcript.lock().unwrap() = Some(Err("provider down".into()));
    *h.agents.speech.lock().unwrap() = Some(speech);
    h.adapter("telegram")
        .media
        .lock()
        .unwrap()
        .insert("voice-1".into(), (Some(100), 1, 100));
    h.send("telegram", with_attachment(MediaKind::Voice, "voice-1", ""))
        .await;
    eventually("a turn", || !h.agents.texts().is_empty()).await;
    let text = h.agents.texts().remove(0);
    let dir = std::fs::canonicalize(media_dir(&h)).unwrap();
    assert!(
        text.starts_with("[The user sent a voice message: ") && text.ends_with("(duration: 7s)]"),
        "{text}"
    );
    let cached = std::fs::read_dir(&dir).unwrap().flatten().next().unwrap();
    assert!(
        text.contains(&*cached.file_name().to_string_lossy()),
        "{text}"
    );
}

#[tokio::test]
async fn a_transcript_is_prepended_and_echoed_once() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let speech = Arc::new(FakeSpeech::default());
    *speech.transcript.lock().unwrap() = Some(Ok("book a table".into()));
    *h.agents.speech.lock().unwrap() = Some(speech);
    h.adapter("telegram")
        .media
        .lock()
        .unwrap()
        .insert("voice-2".into(), (Some(100), 1, 100));
    h.send(
        "telegram",
        with_attachment(MediaKind::Voice, "voice-2", "for two"),
    )
    .await;
    said(&h.adapter("telegram"), "Heard: \"book a table\"").await;
    eventually("a turn", || !h.agents.texts().is_empty()).await;
    assert_eq!(h.agents.texts(), ["book a table\nfor two"]);
}

#[tokio::test]
async fn streamed_reply_audio_is_not_played_twice() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        streaming_tts: true,
        media_out: [MediaKind::Voice].into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    let speech = Arc::new(FakeSpeech::default());
    *speech.transcript.lock().unwrap() = Some(Ok("stream".into()));
    *h.agents.speech.lock().unwrap() = Some(Arc::clone(&speech) as _);
    let adapter = h.adapter("telegram");
    adapter
        .media
        .lock()
        .unwrap()
        .insert("voice-3".into(), (Some(10), 1, 10));
    h.send("telegram", with_attachment(MediaKind::Voice, "voice-3", ""))
        .await;
    said(&adapter, "hello world, this is a streamed answer").await;
    eventually("the audio finished", || {
        adapter
            .audio
            .lock()
            .unwrap()
            .iter()
            .any(|op| matches!(op, AudioOp::Finish { .. }))
    })
    .await;
    let ops = adapter.audio.lock().unwrap().clone();
    assert!(matches!(ops[0], AudioOp::Begin { .. }), "{ops:?}");
    assert!(ops.iter().any(|op| matches!(op, AudioOp::Write { .. })));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let voice_files = adapter
        .all()
        .iter()
        .filter(|d| matches!(d, Did::Send(m) if m.attachments.iter().any(|a| a.as_voice)))
        .count();
    assert_eq!(voice_files, 0);
}

#[tokio::test]
async fn without_streaming_audio_a_voice_note_gets_a_spoken_reply_file() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        media_out: [MediaKind::Voice].into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    let speech = Arc::new(FakeSpeech::default());
    *speech.transcript.lock().unwrap() = Some(Ok("hi there".into()));
    *h.agents.speech.lock().unwrap() = Some(Arc::clone(&speech) as _);
    let adapter = h.adapter("telegram");
    adapter
        .media
        .lock()
        .unwrap()
        .insert("voice-4".into(), (Some(10), 1, 10));
    h.send("telegram", with_attachment(MediaKind::Voice, "voice-4", ""))
        .await;
    eventually("a voice reply", || {
        adapter
            .all()
            .iter()
            .any(|d| matches!(d, Did::Send(m) if m.attachments.iter().any(|a| a.as_voice)))
    })
    .await;
    assert_eq!(*speech.spoken.lock().unwrap(), ["echo: hi there"]);
    // A typed message gets no spoken reply (mirror mode).
    h.dm("telegram", "1", "typed").await;
    said(&adapter, "echo: typed").await;
    assert_eq!(speech.spoken.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_attachment_outside_the_allowed_roots_is_withheld_with_a_note() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let profile = h.home.path().join("profiles").join("default");
    let secrets = profile.join("secrets.toml");
    std::fs::write(&secrets, "TOKEN = \"x\"").unwrap();
    let media = media_dir(&h);
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("chart.png"), "png").unwrap();
    let reply = format!(
        "say:Here you go\nMEDIA:{}\nMEDIA:{}",
        secrets.display(),
        media.join("chart.png").display()
    );
    h.dm("telegram", "1", &reply).await;
    said(
        &h.adapter("telegram"),
        "attachment secrets.toml withheld: path not allowed",
    )
    .await;
    let did = h.adapter("telegram").all();
    let Did::Send(sent) = &did[0] else {
        panic!("{did:?}")
    };
    assert!(sent.text.starts_with("Here you go"));
    let paths: Vec<&str> = sent.attachments.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("chart.png"));
}

/// A workspace dotenv file holds project credentials `read_file` refuses
/// to show; a `MEDIA:` line cannot send it either.
#[tokio::test]
async fn a_workspace_dotenv_file_is_withheld() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let workspace = h.home.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join(".env"), "API_KEY=sk-live").unwrap();
    std::fs::write(workspace.join(".env.local"), "API_KEY=sk-live").unwrap();
    std::fs::write(workspace.join("chart.png"), "png").unwrap();
    *h.agents.cwd.lock().unwrap() = Some(workspace.to_string_lossy().into_owned());
    let reply = format!(
        "say:Here\nMEDIA:{}\nMEDIA:{}\nMEDIA:{}",
        workspace.join(".env").display(),
        workspace.join(".env.local").display(),
        workspace.join("chart.png").display()
    );
    h.dm("telegram", "1", &reply).await;
    said(
        &h.adapter("telegram"),
        "attachment .env withheld: path not allowed",
    )
    .await;
    let did = h.adapter("telegram").all();
    let Did::Send(sent) = &did[0] else {
        panic!("{did:?}")
    };
    assert!(
        sent.text.contains("attachment .env.local withheld"),
        "{}",
        sent.text
    );
    let paths: Vec<&str> = sent.attachments.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(paths[0].ends_with("chart.png"));
}

/// Spec: platform-adapters, `WhatsApp` interactive and rich features:
/// locations travel as structured attachments both ways.
#[tokio::test]
async fn locations_are_read_as_text_and_sent_as_pins_or_links() {
    let mut setup = Setup::new(&open("whatsapp"));
    let mut pins = Descriptor::new("whatsapp", 4096);
    pins.media_in
        .extend([MediaKind::Location, MediaKind::LiveLocation]);
    pins.media_out.insert(MediaKind::Location);
    setup.descriptors.push(pins);
    setup.profiles[0]
        .1
        .push_str("\n[gateway.platforms.telegram]\nenabled = true\nallow_all = true\n");
    let h = Harness::start(setup).await;
    let mut shared = Inbound::text(ChatType::Dm, "1", "1", "meet me");
    let mut pin = Attachment::new(MediaKind::LiveLocation, "geo:-23.5,-46.6");
    pin.name = Some("Paulista".into());
    shared.attachments.push(pin);
    h.send("whatsapp", shared).await;
    eventually("the turn reads the location", || {
        h.agents
            .texts()
            .iter()
            .any(|t| t == "meet me\n[Live location: Paulista (-23.5, -46.6)]")
    })
    .await;
    h.dm(
        "whatsapp",
        "1",
        "say:Here it is\nLOCATION: 48.8584,2.2945 Eiffel Tower",
    )
    .await;
    eventually("the pin goes out as an attachment", || {
        h.adapter("whatsapp").all().iter().any(|d| matches!(d,
            Did::Send(m) if m.attachments.iter().any(|a| a.path == "geo:48.8584,2.2945#Eiffel Tower")))
    })
    .await;
    h.dm("telegram", "1", "say:LOCATION: 48.8584,2.2945 Eiffel Tower")
        .await;
    said(
        &h.adapter("telegram"),
        "📍 Eiffel Tower: https://www.openstreetmap.org/?mlat=48.8584&mlon=2.2945",
    )
    .await;
}
