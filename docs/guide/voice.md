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

## Talking over a reply (barge-in)

With `voice.barge_in` (on by default) the microphone stays open from the
end of your utterance until the reply has finished playing. The first
200 ms of each turn, before anything is played, measure the quiet room;
speech louder than `voice.barge_in_threshold_multiplier` (3.0, 1.0-10.0)
times that level for 160 ms interrupts the reply:

- while the reply is still being written, the turn is interrupted;
- while it plays, playback stops at once (well within 300 ms).

Either way the rest of what you say is recorded and sent as your next
message, with a note telling the model its spoken reply was cut off. A
stop phrase said over a reply ends the conversation. Typing a line during
the playback stops it too (the line is handled as typed input), and the
record key during the playback only silences it. The room level is never
measured from the reply's own sound; raise the multiplier when speakers
next to the microphone trigger it.

## Live transcription

With `voice.stt_streaming = true` and a speech-to-text provider that
streams (`openai`, over its realtime transcription socket with
`gpt-4o-transcribe` unless `stt.openai.model` names a `gpt-` model),
the recording is transcribed while you speak: the text heard so far
follows the level meter on the status line, and the transcript is ready
when the recording ends. The recording itself is always kept until it is
transcribed, so a socket that cannot open or drops mid-utterance costs
nothing: the whole recording is then transcribed the ordinary way.

## The voice turn's model

Spoken messages (here, in the TUI, on the desktop and gateway voice
notes) run on `[auxiliary.voice_chat]` when it names a provider or model
(unset is the session's own model), with reasoning at
`auxiliary.voice_chat.reasoning_effort`, `none` by default (models that
cannot turn reasoning off get their lowest effort). The session keeps its
toolset and system prompt, and the next typed message is back on the
session's model:

```toml
[auxiliary.voice_chat]
provider = "openrouter"
model = "google/gemini-2.5-flash"
```

When the conversation is already longer than the voice model's context
window, voice turns stay on the session's model and a notice says so
once.

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

Recordings are written to the profile's `cache/voice/` (owner-only, mode
0600) for the transcriber and deleted as soon as it has read them. With
`voice.keep_recordings = true` they stay and expire after
`gateway.media.cache_max_age_secs` (swept by the daemon and when a voice
conversation starts). A recording,
voice note or audio attachment over a cloud provider's upload limit
(OpenAI and Groq 25 MiB; Mistral 500 MiB and an hour) is re-encoded to
16 kHz mono Opus first; still too big, it is cut at pauses into pieces
within the limit, transcribed in order and joined. Local providers are
never split. Without ffmpeg only WAV recordings can be cut.

## Wake word

An on-device wake word answers a spoken request without a key press. It
is off until you arm it, and it listens only for the phrase: no audio
leaves the machine before the request itself.

| Command | What it does |
|---|---|
| `/wake on` | Arm the listener (saves `wake.enabled = true`); refused, naming what is missing, until both a speech-to-text and a text-to-speech provider resolve |
| `/wake off` | Stop it (saves `wake.enabled = false`) |
| `/wake status` | Phrase, engine, owner, capture mode, input device, audio host API, anything missing with how to install it |

`/wake` works in the line interface (in process or attached) and the TUI;
the desktop has its ear control. With `wake.enabled = true` a terminal
arms itself when it starts. On the phrase the listener pauses, a new
session opens (`wake.start_new_session = false` continues the current
one), one request is recorded with the usual silence detection, sent,
answered aloud, and listening resumes. While it listens at most 2 s of
audio is held in memory, and none of it is ever written to disk.

Engines (`wake.engine`, default `auto`: the first of these this machine
supports) run as workers of the managed Python runtime (`prometheus
runtimes install uv`; each engine's pinned package is fetched on first
use):

| Engine | Detects | Settings |
|---|---|---|
| `openwakeword` | a model (`wake.openwakeword.model`: a built-in name such as `hey_jarvis`, or an absolute `.onnx`/`.tflite` path) | fires after `wake.confirmation_frames` (3, 1-10) consecutive 80 ms frames over the threshold, so a single ambient spike never fires |
| `sherpa` | the typed `wake.phrase` itself (default "hey prometheus") | the keyword model downloads once into the profile's `cache/speech-models/wake` (`wake.sherpa.model_dir` to use another) |
| `porcupine` | `wake.porcupine.keyword` (a built-in name or an absolute `.ppn` path) | needs the secret `PORCUPINE_ACCESS_KEY` |

`wake.sensitivity` (0.6, 0.0-1.0) is stricter when higher for every
engine. A named engine that cannot run refuses to arm with its
requirement; it is never swapped for another.

Only one listener holds the microphone per machine (the lock
`run/wake.lock`). `wake.surface` (`auto`, `cli`, `tui`, `gui`) picks which
surface may listen; under `auto` the first to arm owns it until it is
stopped, disconnects or exits, and no other surface takes over by itself.
`/wake on` elsewhere names the owner:

```text
the wake listener is owned by the TUI (process 41022, profile default); stop it there first
```

`wake.capture` (`auto`, `local`, `client`) says where the audio comes
from: `local` is the microphone of the machine running the engine;
`client` is a desktop attached to a remote daemon, which streams 16 kHz
mono PCM (`wake.feed`) and records the request on its own microphone once
the daemon reports `wake.detected`; `auto` uses the desktop's microphone
when it arms with client capture. `wake.input_device` picks the
microphone by index or by an unambiguous part of its name.

With `sherpa`, one listener also hears the phrase of every other profile
with wake enabled (`hey <profile>` when it kept the default phrase): the
desktop switches to that profile, and a terminal prints `prometheus -p
<profile>` instead. `wake.profile_routing = false` listens for the
profile's own phrase only.

A microphone that delivers only digital silence for 10 seconds (usually
a missing OS permission) shows up in `/wake status` as "microphone
delivers only silence" with what to allow; `prometheus doctor voice`
reports the same facts as `/wake status`.

## A desktop attached to a remote daemon

With `voice.client_direct = true` (the default) a client attached to a
remote daemon asks it how to reach the profile's speech providers
(`voice.client_config`): a cloud provider holding an API key is reached
directly, so recordings never cross the daemon link and the daemon only
receives the transcript; a provider that runs only on the daemon host (an
on-device engine, a command or plugin provider) or signs in through the
daemon goes through the daemon instead (`voice.transcribe` for
recordings, `voice.speak` for speech). `voice.client_direct = false`
always uses the daemon.

## In chats

On messaging platforms `/voice` sets how this chat's replies are spoken,
overriding `gateway.voice.reply_mode`, and is kept with the chat across
daemon restarts: `/voice on` speaks replies to voice messages, `/voice
tts` speaks every reply, `/voice off` keeps replies text, `/voice status`
shows the mode and where it comes from. Spoken replies are voice bubbles
where the platform has them (Opus in Ogg) and audio files elsewhere.
