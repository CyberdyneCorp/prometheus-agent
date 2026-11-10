# Voice mode

Talk to Prometheus and hear it answer, in the line interface (in process
or attached to the daemon) and in the TUI. Recording, transcription and
speech happen on the machine you sit at; the session's system prompt,
tools and memory never change because of voice mode.

Speech uses the profile's providers: speech-to-text from `[stt]`, the
spoken replies from `[tts]` (see [File and web tools](files-and-web.md)).
Recording needs a recorder on this machine: ffmpeg (macOS and Windows; the
managed copy or one on the `PATH`), or `parecord` with `pactl`
(PulseAudio or PipeWire) or `arecord` (ALSA) on Linux. Playback uses
ffplay, afplay, paplay, pw-play, aplay or PowerShell, converting formats a
player cannot read with ffmpeg.

## Commands

| Command | What it does |
|---|---|
| `/voice` | Toggle voice mode |
| `/voice on` | Turn voice mode on (refused with `No audio input device found` without a microphone, and when no speech-to-text provider resolves) |
| `/voice off` | Turn it off |
| `/voice tts` | Spoken replies on or off (`voice.auto_tts = true` turns them on with voice mode) |
| `/voice status` | Voice and spoken-reply state, record key, input device, STT and TTS providers |

## A conversation

In voice mode press the record key (`voice.record_key`, default
`ctrl+b`; in the TUI it takes precedence over the key's other binding
while voice mode is on). A start beep (880 Hz) plays, the bottom status
line shows a level meter, and the recording ends on its own:

- speech counts once the level (RMS, 0-32767) stays above
  `voice.silence_threshold` (200) for `voice.speech_confirm_ms` (300),
  dips under 200 ms included;
- after that, `voice.silence_duration_ms` (3000) of quiet ends it, so a
  pause between words never cuts you off;
- with no speech by `voice.no_speech_timeout_secs` (15) it ends and
  nothing is sent;
- `voice.max_recording_secs` (120, 5-600) is the hard limit.

A double end beep (660 Hz) plays (`voice.beep_enabled = false` silences
both), the recording is transcribed and sent as your message, the reply is
spoken when spoken replies are on, and recording starts again by itself.
The conversation ends when you press the record key during a recording,
say or type a stop phrase, or after three recordings in a row without
speech.

Stop phrases (`voice.stop_phrases`, default `stop`, `never mind`,
`goodbye`, `cancel`, `that's all`; `[]` disables) end the conversation
only when they are the whole utterance: "stop the docker container" is a
request. Typed outside a voice conversation, "stop" is an ordinary
message. Text typed during a recording runs as typed input.

Speech-to-text models invent text from silence ("Thank you for
watching.", one word repeated). Such transcripts are dropped and count as
a recording without speech (`voice.hallucination_filter = false` keeps
them).

The `[voice]` keys are ordinary configuration, checked when they are set:

```console
$ prometheus config set voice.record_key alt+v
$ prometheus config get voice.record_key
alt+v
$ prometheus config set voice.max_recording_secs 2  # exits 2
error: voice.max_recording_secs: must be between 5 and 600
```

## Spoken replies

The reply is spoken while it is written: text is cut into sentences of at
least 20 characters, Markdown, code blocks, emoji and reasoning blocks are
left out, and sentences play in order without overlapping. `openai` and
`elevenlabs` stream raw PCM, so a sentence starts playing before its audio
is complete; other providers synthesize one file per sentence. Attached
to the daemon (the TUI, or a line interface while the daemon runs) the
reply is spoken from the turn's `assistant.delta` events as the daemon
streams them, so the first sentence plays while the rest is still being
written there too.

## Recordings and long audio

Recordings are written to the profile's `cache/voice/` (owner-only) for
the transcriber and deleted as soon as it has read them. A recording,
voice note or audio attachment over a cloud provider's upload limit
(OpenAI and Groq 25 MiB; Mistral 500 MiB and an hour) is re-encoded to
16 kHz mono Opus first; still too big, it is cut at pauses into pieces
within the limit, transcribed in order and joined. Local providers are
never split. Without ffmpeg only WAV recordings can be cut.

## In chats

On messaging platforms `/voice` sets how this chat's replies are spoken,
overriding `gateway.voice.reply_mode`, and is kept with the chat across
daemon restarts: `/voice on` speaks replies to voice messages, `/voice
tts` speaks every reply, `/voice off` keeps replies text, `/voice status`
shows the mode and where it comes from. Spoken replies are voice bubbles
where the platform has them (Opus in Ogg) and audio files elsewhere.
