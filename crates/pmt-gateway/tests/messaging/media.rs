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
    // The cache also holds the spoken copy of the reply (`speech-N.ogg`,
    // the reply mode speaks to voice messages): the voice note is the
    // file named by its content hash.
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let note = names
        .iter()
        .find(|n| n.len() == 64 + 4 && n[..64].chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no cached voice note in {names:?}"));
    assert!(text.contains(note.as_str()), "{text}");
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

/// A voice-note attachment of a reply reaches a voice-bubble platform as
/// Ogg (spec: web-and-media-tools, `text_to_speech` tool and delivery:
/// Telegram bubble); without a converter it goes as plain audio.
#[tokio::test]
async fn mp3_voice_attachments_are_converted_for_voice_bubbles_or_sent_as_audio() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        media_out: [MediaKind::Voice, MediaKind::Audio].into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    *h.agents.speech.lock().unwrap() = Some(Arc::new(FakeSpeech::default()));
    let media = media_dir(&h);
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("speech.mp3"), b"ID3audio").unwrap();
    let reply = format!(
        "say:Listen\nMEDIA:{} [[as_voice]]",
        media.join("speech.mp3").display()
    );
    h.dm("telegram", "1", &reply).await;
    said(&h.adapter("telegram"), "Listen").await;
    let sent = |h: &Harness| {
        h.adapter("telegram")
            .all()
            .into_iter()
            .find_map(|d| match d {
                Did::Send(m) if !m.attachments.is_empty() => Some(m.attachments[0].clone()),
                _ => None,
            })
            .unwrap()
    };
    let file = sent(&h);
    assert!(file.as_voice, "{file:?}");
    assert!(file.path.ends_with("speech.voice.ogg"), "{file:?}");

    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        media_out: [MediaKind::Voice, MediaKind::Audio].into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let plain = Harness::start(setup).await;
    let media = media_dir(&plain);
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("speech.mp3"), b"ID3audio").unwrap();
    let reply = format!(
        "say:Listen\nMEDIA:{} [[as_voice]]",
        media.join("speech.mp3").display()
    );
    plain.dm("telegram", "1", &reply).await;
    said(&plain.adapter("telegram"), "Listen").await;
    let file = sent(&plain);
    assert!(!file.as_voice, "{file:?}");
    assert!(file.path.ends_with("speech.mp3"));
}

/// `/voice tts` is the chat's choice: stored with the chat, it survives a
/// daemon restart and makes even a typed message get a voice bubble
/// (spec: voice-mode, Gateway per-chat voice reply command).
#[tokio::test]
async fn a_chats_voice_choice_survives_a_restart() {
    let telegram = || {
        let mut setup = Setup::new(&open("telegram"));
        setup.descriptors.push(Descriptor {
            media_out: [MediaKind::Voice, MediaKind::Audio].into_iter().collect(),
            ..Descriptor::new("telegram", 4096)
        });
        setup
    };
    let h = Harness::start(telegram()).await;
    h.dm("telegram", "1", "/voice status").await;
    said(
        &h.adapter("telegram"),
        "Voice replies: on (spoken to voice messages) (gateway.voice.reply_mode)",
    )
    .await;
    h.dm("telegram", "1", "/voice tts").await;
    said(
        &h.adapter("telegram"),
        "Voice replies: tts (every reply spoken) for this chat.",
    )
    .await;
    let home = h.stop().await;
    let h = Harness::start_in(home, telegram()).await;
    let speech = Arc::new(FakeSpeech::default());
    *h.agents.speech.lock().unwrap() = Some(Arc::clone(&speech) as _);
    let adapter = h.adapter("telegram");
    h.dm("telegram", "1", "typed after the restart").await;
    eventually("a voice bubble", || {
        adapter
            .all()
            .iter()
            .any(|d| matches!(d, Did::Send(m) if m.attachments.iter().any(|a| a.as_voice)))
    })
    .await;
    assert_eq!(
        *speech.spoken.lock().unwrap(),
        ["echo: typed after the restart"]
    );
    h.dm("telegram", "1", "/voice off").await;
    said(&adapter, "Voice replies: off (text only) for this chat.").await;
    h.dm("telegram", "1", "quiet now").await;
    said(&adapter, "echo: quiet now").await;
    assert_eq!(speech.spoken.lock().unwrap().len(), 1);
}

/// A platform without voice bubbles gets the spoken reply as an audio
/// file.
#[tokio::test]
async fn without_voice_bubbles_a_spoken_reply_is_an_audio_file() {
    let mut setup = Setup::new(&open("slack"));
    setup.descriptors.push(Descriptor {
        media_out: [MediaKind::Audio].into_iter().collect(),
        ..Descriptor::new("slack", 4000)
    });
    let h = Harness::start(setup).await;
    let speech = Arc::new(FakeSpeech::default());
    *h.agents.speech.lock().unwrap() = Some(Arc::clone(&speech) as _);
    let adapter = h.adapter("slack");
    h.dm("slack", "1", "/voice tts").await;
    said(&adapter, "for this chat.").await;
    h.dm("slack", "1", "read this to me").await;
    eventually("an audio attachment", || {
        adapter.all().iter().any(|d| {
            matches!(d, Did::Send(m) if m.attachments.iter().any(|a| !a.as_voice && a.path.contains("speech-")))
        })
    })
    .await;
}
