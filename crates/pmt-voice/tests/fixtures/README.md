# Voice fixtures

Recorded speech for the voice-mode tests: 16 kHz mono 16-bit PCM WAV.

- `question.wav`: "What time is it in Lisbon right now?" between short stretches of room noise.
- `pause.wav`: "Turn on the", a one-second pause, then "kitchen lights, please."
- `room.wav`: two seconds of room noise (RMS about 25) and nobody speaking.

The speech was made with the macOS `say` voice at 16 kHz and mixed with
Gaussian room noise (sigma 25); the fixture microphone follows each file
with digital silence, as a quiet room would.
