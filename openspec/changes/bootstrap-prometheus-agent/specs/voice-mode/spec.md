# Voice Mode

## Purpose
Voice mode lets a user talk to Prometheus and hear it answer: push-to-talk and continuous hands-free conversation on the CLI, TUI and desktop app, an always-off-by-default on-device wake word, live voice sessions in Discord voice channels, and a full-duplex realtime voice mode on the desktop. The key decision is one voice pipeline (capture, silence detection, speech-to-text, turn, sentence-streamed text-to-speech, barge-in) shared by every surface and driven by the `voice` and `wake` config sections, with speech providers taken from the profile's `stt`/`tts` provider configuration (owned by `web-and-media-tools`) so no surface carries its own provider logic and no audio leaves the machine before the user speaks a command.

## ADDED Requirements

### Requirement: Voice mode toggle and commands
The system SHALL provide the slash command `/voice` with subcommands `on`, `off`, `tts` (toggle spoken replies) and `status` on the `cli`, `tui` and `desktop` surfaces, where bare `/voice` toggles voice mode; `/voice on` SHALL refuse with "No audio input device found" when no input device is available and with "speech-to-text is not configured; run `prometheus tools` (Voice)" when no STT provider resolves, and `/voice status` SHALL print the voice state, spoken-reply state, record key, input device, STT provider and TTS provider; voice mode SHALL never change the system prompt, toolset or memory snapshot of the running session.
**Milestone:** M2

#### Scenario: Enable without a microphone
- **GIVEN** a host with no audio input device
- **WHEN** the user runs `/voice on`
- **THEN** the system SHALL print "No audio input device found" and voice mode SHALL stay off

#### Scenario: Status lists providers
- **GIVEN** voice mode on with `stt` provider `local` and `tts` provider `edge`
- **WHEN** the user runs `/voice status`
- **THEN** the output SHALL include `voice: on`, `record key: ctrl+b`, `stt: local` and `tts: edge`

### Requirement: Push-to-talk recording and continuous loop
In voice mode the system SHALL start recording when the user presses `voice.record_key` (default `ctrl+b`), play a start beep (880 Hz) and an end double beep (660 Hz) unless `voice.beep_enabled = false`, show a live input level meter, stop at `voice.max_recording_secs` (default 120, range 5-600), transcribe and submit the recording as a user message, and then automatically restart recording after the reply finishes; the continuous loop SHALL end when the record key is pressed during a recording or after 3 consecutive recordings that detected no speech.
**Milestone:** M2

#### Scenario: Continuous conversation
- **GIVEN** voice mode on
- **WHEN** the user presses Ctrl+B, speaks, and stops speaking
- **THEN** the system SHALL submit the transcript, speak the reply when spoken replies are on, and start recording again without a key press

#### Scenario: Silence ends the loop
- **WHEN** 3 consecutive recordings contain no detected speech
- **THEN** the system SHALL leave continuous mode and print that listening stopped

### Requirement: Two-stage silence detection
The system SHALL end a recording by two-stage voice activity detection: speech is confirmed once the input RMS (0-32767 scale) stays above `voice.silence_threshold` (default 200) for at least `voice.speech_confirm_ms` (default 300) tolerating dips shorter than 200 ms, after which `voice.silence_duration_ms` (default 3000) of continuous sub-threshold audio ends the recording; a recording in which no speech is confirmed within `voice.no_speech_timeout_secs` (default 15) SHALL end without being transcribed.
**Milestone:** M2

#### Scenario: Pause between syllables
- **GIVEN** confirmed speech
- **WHEN** the user pauses for 1 second mid-sentence and continues
- **THEN** the system SHALL keep recording

#### Scenario: Nobody speaks
- **WHEN** a recording starts and no audio exceeds the threshold for 15 seconds
- **THEN** the system SHALL stop the recording and submit nothing

### Requirement: Spoken and typed stop phrases
The system SHALL end an active voice conversation when the whole transcribed utterance, compared case-insensitively with surrounding punctuation and whitespace ignored, equals one of `voice.stop_phrases` (default `["stop", "never mind", "goodbye", "cancel", "that's all"]`; `[]` disables), SHALL NOT send that utterance to the agent, and SHALL treat a typed message equal to a stop phrase the same way only while a voice conversation is active on that surface.
**Milestone:** M2

#### Scenario: Partial match is a request
- **WHEN** the user says "stop the docker container"
- **THEN** the system SHALL send the utterance to the agent and the voice conversation SHALL continue

#### Scenario: Typed stop outside voice
- **GIVEN** no voice conversation is active
- **WHEN** the user types "stop"
- **THEN** the system SHALL send "stop" to the agent as an ordinary message

### Requirement: Transcription hallucination filter
The system SHALL discard a transcript that, after trimming and case folding, matches a built-in list of known silence hallucinations (at least "thank you for watching", "thanks for watching", "subscribe", "please subscribe", "you", "bye" and their common non-English equivalents) or consists of one phrase repeated three or more times, and SHALL treat a discarded transcript as a recording with no speech; `voice.hallucination_filter = false` SHALL disable the filter.
**Milestone:** M2

#### Scenario: Phantom outro dropped
- **WHEN** a silent recording transcribes as "Thank you for watching."
- **THEN** the system SHALL submit nothing and count the recording as no speech

### Requirement: Live streaming transcription
When `voice.stt_streaming = true` (default `false`) and the profile's STT provider declares streaming support, the system SHALL transcribe while the user speaks, showing partial text in the CLI and TUI input placeholder and the desktop dictation pill, and SHALL have the final transcript ready when the recording ends; when the streaming session cannot open or fails mid-recording the system SHALL fall back to transcribing the full recording and SHALL NOT lose the utterance.
**Milestone:** M3

#### Scenario: Streaming failure falls back
- **GIVEN** `voice.stt_streaming = true` and the realtime STT socket drops mid-utterance
- **WHEN** the recording ends
- **THEN** the system SHALL transcribe the saved recording through the file path and submit the result

### Requirement: Sentence-streamed spoken replies
When spoken replies are on, the system SHALL speak the reply while it generates by buffering text deltas into complete sentences of at least 20 characters, stripping Markdown, code blocks, emoji and reasoning blocks before synthesis, streaming raw PCM for providers that offer chunked audio and synthesizing per sentence otherwise, and playing sentences in order without overlap; `voice.auto_tts = true` (default `false`) SHALL turn spoken replies on whenever voice mode starts.
**Milestone:** M2

#### Scenario: First sentence plays early
- **GIVEN** a reply that streams three sentences over 6 seconds
- **WHEN** the first sentence completes
- **THEN** the system SHALL begin playing it before the remaining text has been generated

#### Scenario: Code is not read aloud
- **WHEN** a spoken reply contains a fenced code block
- **THEN** the system SHALL omit the code block from the synthesized speech

### Requirement: Barge-in
While `voice.barge_in = true` (default) the microphone SHALL stay live from the end of the user's utterance until the reply finishes playing: speech detected during generation SHALL interrupt the turn and become the next user message, speech detected during playback SHALL stop playback immediately and be submitted, and typing or pressing the record key SHALL stop playback on every surface; the speech trigger SHALL be `voice.barge_in_threshold_multiplier` (default 3.0, range 1.0-10.0) times a noise floor calibrated from the quiet room at turn start, never from playback audio, and the next user message SHALL carry a note telling the model its spoken reply was cut off.
**Milestone:** M3

#### Scenario: Talk over playback
- **GIVEN** the agent is speaking a long reply
- **WHEN** the user starts speaking above the barge-in threshold
- **THEN** the system SHALL stop playback within 300 ms, record the user's utterance and submit it with the interruption note

#### Scenario: Stop phrase mid-generation
- **WHEN** the user says "stop" while the agent is generating
- **THEN** the system SHALL interrupt the turn and end the voice conversation

### Requirement: Long recordings and provider upload limits
Before cloud transcription the system SHALL compare the audio against the active STT provider's declared per-request limits (bytes and duration), SHALL first re-encode an over-limit recording to 16 kHz mono compressed audio, and when still over the limit SHALL split it at pauses into pieces within the limit, transcribe the pieces in order and join the text; local providers SHALL never be split, and this SHALL apply equally to voice-mode recordings, gateway voice notes and audio attachments.
**Milestone:** M2

#### Scenario: Long voice note split
- **GIVEN** an STT provider with a 25 MB per-request limit
- **WHEN** a 90-minute voice note that is 60 MB after re-encoding arrives
- **THEN** the system SHALL transcribe it in pieces split at pauses and pass one joined transcript to the agent

### Requirement: Voice turn model and reasoning
Chained voice turns (CLI, TUI and desktop voice conversations and gateway voice notes) SHALL run on the `[auxiliary.voice_chat]` route when it is set (default `auto` = the session's model) with reasoning off by default (`auxiliary.voice_chat.reasoning_effort = "none"`, or the model's lowest accepted effort when it cannot disable reasoning), keeping the session's full toolset; the next typed message SHALL return to the session's model, and a voice turn whose context exceeds the voice model's window SHALL run on the session's model with a one-time notice instead of forcing compression.
**Milestone:** M3

#### Scenario: Faster model for speech
- **GIVEN** `[auxiliary.voice_chat] provider = "openrouter"` and a session on a slower main model
- **WHEN** the user speaks a request and then types a follow-up
- **THEN** the spoken turn SHALL use the voice route and the typed follow-up SHALL use the session's main route

### Requirement: Gateway per-chat voice reply command
The gateway SHALL provide `/voice` in chats with subcommands `on` (speak replies only to voice messages), `tts` (speak every reply), `off` (text only) and `status`, overriding `gateway.voice.reply_mode` for that chat and persisting the choice in the session store across daemon restarts; platforms SHALL deliver spoken replies as native voice bubbles (Opus/OGG) where the descriptor's `media_out` includes `voice` and as an audio file attachment otherwise.
**Milestone:** M2

#### Scenario: Persisted chat voice mode
- **GIVEN** a Telegram user runs `/voice tts`
- **WHEN** the daemon restarts and the user sends a text message
- **THEN** the reply SHALL include a voice bubble

### Requirement: Discord voice channel sessions
The Discord adapter SHALL support `/voice join` (alias `/voice channel`), `/voice leave` and `/voice status` in a text channel, joining the voice channel the invoking user is in (refusing with "join a voice channel first" otherwise) and binding that text channel; while joined it SHALL receive each speaker's audio independently, end an utterance after 1500 ms of silence following at least 500 ms of speech, transcribe it, post `[Voice] @<user>: <transcript>` to the bound text channel, run the turn in that channel's session, and send the reply both as text and as speech in the voice channel; it SHALL require the `Connect` and `Speak` permissions and report a missing one by name.
**Milestone:** M3

#### Scenario: Spoken question answered in voice
- **GIVEN** the bot joined a voice channel via `/voice join` from `#general`
- **WHEN** an allowed user asks a question aloud
- **THEN** `#general` SHALL show the `[Voice]` transcript and the text reply and the bot SHALL speak the reply in the voice channel

#### Scenario: Join without being in voice
- **WHEN** a user not in any voice channel runs `/voice join`
- **THEN** the bot SHALL reply "join a voice channel first" and join nothing

### Requirement: Voice channel echo suppression, access control and rebinding
While playing speech in a voice channel the Discord adapter SHALL pause capture so the bot never transcribes its own output; it SHALL process audio only from speakers authorized by the platform's allowlist or allowed roles, re-checking role membership on every utterance and silently ignoring others; and running `/voice join` from another text channel SHALL move the binding there and drop speech captured before the move rather than posting it to the new channel.
**Milestone:** M3

#### Scenario: Unauthorized speaker ignored
- **GIVEN** a voice channel with an allowed user and a non-allowed user
- **WHEN** the non-allowed user speaks
- **THEN** the system SHALL neither transcribe nor respond to that audio

#### Scenario: No self-echo
- **WHEN** the bot plays its reply in the voice channel
- **THEN** no transcript of the bot's own speech SHALL be produced

### Requirement: Wake word listener
The system SHALL provide an on-device wake word listener, off by default (`wake.enabled = false`), toggled with `/wake on|off|status` or the desktop ear control; when armed it SHALL listen only for the wake phrase without sending audio anywhere, and on detection SHALL pause itself, start a new session when `wake.start_new_session = true` (default) or continue the current one otherwise, record one utterance with voice-mode silence detection, submit it, speak the reply, and resume listening; `/wake on` SHALL refuse to arm until both an STT and a TTS provider resolve, naming what is missing, and the toggle SHALL persist `wake.enabled` as an explicit user configuration change.
**Milestone:** M3

#### Scenario: Hands-free request
- **GIVEN** `wake.enabled = true` and the CLI open
- **WHEN** the user says "hey prometheus, what's on my calendar"
- **THEN** the system SHALL open a new session, transcribe the command, answer aloud and then resume listening

#### Scenario: Refuses without TTS
- **GIVEN** no TTS provider resolves
- **WHEN** the user runs `/wake on`
- **THEN** the system SHALL refuse to arm and name the missing TTS provider

### Requirement: Wake engines and false-trigger control
`wake.engine` SHALL accept `auto` (default), `openwakeword`, `sherpa` and `porcupine`, where `auto` selects the first engine supported on the host in that order and an explicitly configured engine that is unsupported or missing SHALL fail to arm with its requirement error instead of switching engines; `wake.phrase` SHALL default to "hey prometheus" (the detection key for `sherpa`, a display label for model-keyed engines), `wake.sensitivity` (default 0.6, range 0.0-1.0) SHALL mean stricter-when-higher for every engine, `wake.confirmation_frames` (default 3, range 1-10, `openwakeword` only) SHALL require that many consecutive over-threshold frames before firing, and `wake.porcupine.keyword` and `wake.openwakeword.model` SHALL accept a built-in name or an absolute model path, with the Porcupine access key read from the secrets store as `PORCUPINE_ACCESS_KEY`.
**Milestone:** M3

#### Scenario: Ambient spike ignored
- **GIVEN** `engine = "openwakeword"` and `confirmation_frames = 3`
- **WHEN** a single 80 ms frame exceeds the threshold during background conversation
- **THEN** the system SHALL NOT fire the wake word

#### Scenario: Explicit engine not silently swapped
- **GIVEN** `engine = "porcupine"` and no `PORCUPINE_ACCESS_KEY` secret
- **WHEN** the user runs `/wake on`
- **THEN** the system SHALL refuse with an error naming `PORCUPINE_ACCESS_KEY` and SHALL NOT arm another engine

### Requirement: Single wake listener ownership across surfaces
`wake.surface` SHALL accept `auto` (default; the first eligible local surface to arm owns the listener), `cli`, `tui` or `gui`; only one listener SHALL hold the microphone per machine at a time, enforced by a lock under the home's `run/` directory, ownership SHALL be sticky until the owner stops, disconnects or exits, and the system SHALL NOT fail over to another surface silently; `/wake status` on a non-owning surface SHALL name the owning surface and process.
**Milestone:** M3

#### Scenario: Second surface does not steal the mic
- **GIVEN** the TUI owns the wake listener
- **WHEN** the CLI runs `/wake on`
- **THEN** the CLI SHALL report that the TUI owns the listener and SHALL NOT open the microphone

### Requirement: Wake capture location for remote clients
`wake.capture` SHALL accept `auto` (default), `local` and `client`: `local` opens the microphone on the host running the engine, `client` expects 16 kHz mono 16-bit PCM frames from an attached client via the RPC `wake.feed` over the authenticated client connection, and `auto` uses client capture when the desktop arms with `client_capture = true` and local capture otherwise; detection SHALL emit the event `wake.detected {profile, phrase}`, after which the desktop records the command on its own microphone, and wake audio SHALL never be sent to a third-party service.
**Milestone:** M3

#### Scenario: Headless backend with desktop microphone
- **GIVEN** a remote daemon host without an input device and the desktop arming wake with `client_capture = true`
- **WHEN** the user says the wake phrase near the desktop
- **THEN** the daemon SHALL detect it from `wake.feed` frames and emit `wake.detected`

### Requirement: Wake word profile routing
With the `sherpa` engine one listener SHALL enroll every profile whose config has `wake.enabled = true`, each with its `wake.phrase` or `hey <profile name>` when unset, and on detection of another profile's phrase the desktop SHALL switch to that profile and open a new voice session there; `wake.profile_routing = false` on the listening profile SHALL restrict detection to its own phrase, and single-profile CLI and TUI processes SHALL print `prometheus -p <profile>` instead of routing.
**Milestone:** M3

#### Scenario: Wake a different profile
- **GIVEN** profiles `default` and `coder` both with wake enabled and the desktop listening
- **WHEN** the user says "hey coder"
- **THEN** the desktop SHALL switch to profile `coder` and start a new voice session in it

### Requirement: Wake and voice diagnostics
`/wake status` and `prometheus doctor` SHALL report the wake phrase, engine, owner surface, capture mode, selected input device and audio host API, and missing engine or audio dependencies with the install command; when an armed listener receives only digital silence for 10 seconds the system SHALL report "microphone delivers only silence" with the OS permission hint, and `wake.input_device` SHALL accept a device index or an unambiguous device-name substring (an ambiguous substring fails with the matching device names).
**Milestone:** M3

#### Scenario: Silent microphone reported
- **GIVEN** the backend process lacks OS microphone permission and its stream returns zeros
- **WHEN** the user runs `/wake status`
- **THEN** the output SHALL include "microphone delivers only silence" and the permission hint

### Requirement: Desktop voice controls
The desktop app SHALL provide a composer microphone for one-shot dictation into the input box, a read-aloud control on each assistant message, a voice-conversation button that runs the chained voice loop with barge-in, and a wake ear indicator that is solid while the wake listener is armed and shows an error hint when arming failed; these controls SHALL be hidden when no STT (dictation) or TTS (read aloud) provider resolves for the active profile.
**Milestone:** M3

#### Scenario: Read a message aloud
- **WHEN** the user clicks read-aloud on an assistant message
- **THEN** the desktop SHALL speak that message with the profile's TTS provider and stop when clicked again

### Requirement: Client-direct voice for remote daemons
When the desktop is attached to a remote daemon and `voice.client_direct = true` (default), the desktop SHALL obtain the profile's resolved STT and TTS settings and credentials via the RPC `voice.client_config`, send recordings straight to the STT provider, send only the resulting text to the daemon, and synthesize replies locally from the streamed text; credentials SHALL be held in memory for the voice session only, and providers that run only on the daemon host (local models, command or plugin providers) or OAuth-backed credentials SHALL use the relay path through the daemon instead.
**Milestone:** M3

#### Scenario: Audio never crosses the daemon link
- **GIVEN** a remote daemon with a cloud STT and TTS provider
- **WHEN** the user holds a desktop voice conversation
- **THEN** the daemon SHALL receive only transcript text and SHALL send no audio

#### Scenario: Host-only provider relays
- **GIVEN** the profile's STT provider is a local model on the daemon host
- **WHEN** the desktop records a command
- **THEN** the desktop SHALL upload the audio to the daemon's transcription endpoint

### Requirement: Realtime full-duplex voice chat mode (upgrade)
`voice.chat_mode` SHALL accept `chained` (default) or `realtime`; in `realtime` mode on the desktop a single full-duplex realtime voice model session (configured by `voice.realtime.provider`, `voice.realtime.model`, `voice.realtime.voice` and optional `voice.realtime.instructions`) SHALL handle listening, interruptions and speech, SHALL be created by the daemon so the client receives only a session id and connection answer and never the API key, and SHALL hand every substantive request to the Prometheus agent through a single delegated tool whose result it speaks; the voice-layer cost SHALL be recorded in the usage ledger separately from the agent turn, and CLI and TUI SHALL keep the chained loop.
**Milestone:** M3

#### Scenario: Realtime voice delegates work
- **GIVEN** `voice.chat_mode = "realtime"` on the desktop
- **WHEN** the user asks aloud to summarize a file in the workspace
- **THEN** the realtime session SHALL call the delegated agent tool, the agent SHALL read the file with its normal tools and grants, and the realtime voice SHALL speak the result

#### Scenario: Key stays on the daemon
- **WHEN** the desktop opens a realtime voice session
- **THEN** no provider API key SHALL be sent to the desktop process

### Requirement: Recording retention and privacy (upgrade)
Voice-mode and wake recordings SHALL be written only to the owning profile's `cache/voice/` with mode 0600 and deleted immediately after successful transcription unless `voice.keep_recordings = true`, in which case they follow `gateway.media.cache_max_age_secs`; the wake listener SHALL keep no more than 2 seconds of rolling audio in memory and SHALL never persist pre-wake audio.
**Milestone:** M3

#### Scenario: Recording removed after use
- **GIVEN** `voice.keep_recordings = false`
- **WHEN** a recording is transcribed successfully
- **THEN** no file for it SHALL remain under `cache/voice/`
