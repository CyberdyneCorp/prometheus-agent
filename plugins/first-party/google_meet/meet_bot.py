"""The meeting bot: a detached process (started by ``process_manager``) that
joins one Google Meet call in Playwright's Chromium and scrapes the live
captions into a transcript.

It runs with the browser runtime's interpreter, as a script: its sibling
leaf modules (``meeturl``, ``jsonfile``, ``audio_bridge``, ``realtime``) are
imported from this directory. Configured by ``PROMETHEUS_MEET_*`` variables;
the filesystem is its only channel: ``status.json``, ``transcript.txt``,
``say_queue.jsonl`` and ``leave.request`` in its meeting directory.

No meeting audio is decoded: a MutationObserver reads Meet's caption region
(lossy and language-dependent, but deterministic and free of speech-to-text).

``meet_bot.py --auth <auth.json> <trigger>`` opens a visible browser on the
Google sign-in page and saves its session state once ``<trigger>`` exists.
"""

from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Optional

from jsonfile import write_json
from meeturl import is_meet_url, meeting_id

LEAVE_REQUEST = "leave.request"
_FFMPEG_MISSING = "ffmpeg not found: install it (e.g. `brew install ffmpeg`) for realtime on macOS"


def _quiet(fn, *args, **kwargs):
    """Calls ``fn``, swallowing any exception (best-effort browser steps)."""
    try:
        return fn(*args, **kwargs)
    except Exception:  # noqa: BLE001 - Meet's page changes; a miss is not fatal
        return None


# status.json key -> state attribute, initial value.
_STATUS_FIELDS = (
    ("meetingId", "meeting_id", None), ("url", "url", None), ("inCall", "in_call", False),
    ("captioning", "captioning", False), ("captionsEnabledAttempted", "captions_enabled_attempted", False),
    ("lobbyWaiting", "lobby_waiting", False), ("joinAttemptedAt", "join_attempted_at", None),
    ("joinedAt", "joined_at", None), ("lastCaptionAt", "last_caption_at", None),
    ("transcriptLines", "transcript_lines", 0), ("error", "error", None), ("exited", "exited", False),
    ("realtime", "realtime", False), ("realtimeReady", "realtime_ready", False),
    ("realtimeDevice", "realtime_device", None), ("audioBytesOut", "audio_bytes_out", 0),
    ("lastAudioOutAt", "last_audio_out_at", None), ("lastBargeInAt", "last_barge_in_at", None),
    ("leaveReason", "leave_reason", None), ("micState", "mic_state", None))


class BotState:
    """The bot's state, written to ``status.json`` on every change."""

    def __init__(self, out_dir: Path, meeting: str, url: str) -> None:
        for _key, attr, default in _STATUS_FIELDS:
            setattr(self, attr, default)
        self.meeting_id, self.url = meeting, url
        self.transcript_path = out_dir / "transcript.txt"
        self.status_path = out_dir / "status.json"
        self._seen: set[str] = set()
        out_dir.mkdir(parents=True, exist_ok=True)
        self._flush()

    def record_caption(self, speaker: str, text: str) -> None:
        """Appends a caption line unless this (speaker, text) was seen."""
        speaker, text = (speaker or "").strip() or "Unknown", (text or "").strip()
        key = f"{speaker}|{text}"
        if not text or key in self._seen:
            return
        self._seen.add(key)
        self.transcript_lines += 1
        self.last_caption_at = time.time()
        stamp = time.strftime("%H:%M:%S", time.localtime(self.last_caption_at))
        with self.transcript_path.open("a", encoding="utf-8") as f:
            f.write(f"[{stamp}] {speaker}: {text}\n")
        self._flush()

    def set(self, **changes) -> None:
        for attr, value in changes.items():
            setattr(self, attr, value)
        self._flush()

    def _flush(self) -> None:
        data = {key: getattr(self, attr) for key, attr, _default in _STATUS_FIELDS}
        data.update(transcriptPath=str(self.transcript_path), pid=os.getpid())
        write_json(self.status_path, data, mode=0o644)


# Injected into the Meet tab: a MutationObserver on the caption region
# queues {speaker, text}; ``window.__prometheusMeetDrain()`` pulls them.
_CAPTION_OBSERVER_JS = r"""
(() => {
  if (window.__prometheusMeetInstalled) return;
  window.__prometheusMeetInstalled = true;
  window.__prometheusMeetQueue = [];
  const captionSelector = '[role="region"][aria-label*="aption" i], div[jsname="YSxPC"], div[jsname="tgaKEf"]';
  function push(speaker, text) {
    if (!text || !text.trim()) return;
    window.__prometheusMeetQueue.push({ts: Date.now(), speaker: (speaker || '').trim(), text: text.trim()});
  }
  function scan(root) {
    const rows = root.querySelectorAll('div[jsname="dsyhDe"], div.CNusmb, div.TBMuR');
    if (rows.length) {
      rows.forEach((row) => {
        const who = row.querySelector('div.KcIKyf, div.zs7s8d, span[jsname="YSxPC"]');
        const what = row.querySelector('div.bh44bd, span[jsname="tgaKEf"], div.iTTPOb');
        push(who ? who.innerText : '', what ? what.innerText : row.innerText);
      });
      return;
    }
    push('', (root.innerText || '').split('\n').filter(Boolean).pop());
  }
  function attach() {
    const el = document.querySelector(captionSelector);
    if (!el) return false;
    new MutationObserver(() => scan(el)).observe(el, {childList: true, subtree: true, characterData: true});
    scan(el);
    return true;
  }
  if (!attach()) {
    const timer = setInterval(() => { if (attach()) clearInterval(timer); }, 1500);
  }
  window.__prometheusMeetDrain = () => {
    const out = window.__prometheusMeetQueue.slice();
    window.__prometheusMeetQueue = [];
    return out;
  };
})();
"""

# Meet binds the caption toggle to the `c` key; clicking is too brittle.
_ENABLE_CAPTIONS_JS = (
    "(() => { document.body.dispatchEvent(new KeyboardEvent('keydown', "
    "{key: 'c', code: 'KeyC', keyCode: 67, which: 67, bubbles: true})); return true; })();")

_LEAVE_CALL_JS = "() => { const b = document.querySelector('button[aria-label*=\"eave call\"]'); if (b) b.click(); }"

# Past the lobby: the leave button, the caption region or the participant list.
_ADMISSION_PROBE_JS = r"""
(() => {
  if (document.querySelector('button[aria-label*="eave call" i]')) return true;
  if (window.__prometheusMeetInstalled && document.querySelector(
      '[role="region"][aria-label*="aption" i], div[jsname="YSxPC"], div[jsname="tgaKEf"]')) return true;
  return !!document.querySelector('[aria-label*="articipants" i]');
})();
"""

# What Meet shows (in English) when the host denies or removes a guest.
_DENIED_PROBE_JS = r"""
(() => {
  const text = document.body ? document.body.innerText || '' : '';
  return /You can't join this video call|You were removed from the meeting|No one responded to your request to join/i.test(text);
})();
"""


def _probe(page, js: str) -> bool:
    return bool(_quiet(page.evaluate, js))


def _visible(locator):
    return _quiet(lambda: locator.first if locator.first.count() and locator.first.is_visible() else None)


def parse_duration(raw: str) -> Optional[float]:
    """``30m`` / ``2h`` / ``90s`` / ``90`` (seconds) as seconds, or None."""
    raw = (raw or "").strip().lower()
    if not raw:
        return None
    unit = {"h": 3600.0, "m": 60.0, "s": 1.0}.get(raw[-1:])
    try:
        return float(raw[:-1]) * unit if unit else float(raw)
    except ValueError:
        return None


def config_from_env() -> SimpleNamespace:
    env = os.environ.get
    out = env("PROMETHEUS_MEET_OUT_DIR", "").strip()
    return SimpleNamespace(
        url=env("PROMETHEUS_MEET_URL", "").strip(),
        out_dir=Path(out) if out else None,
        headed=env("PROMETHEUS_MEET_HEADED", "").lower() in {"1", "true", "yes"},
        auth_state=env("PROMETHEUS_MEET_AUTH_STATE", "").strip(),
        guest_name=env("PROMETHEUS_MEET_GUEST_NAME", "Prometheus Agent"),
        duration_s=parse_duration(env("PROMETHEUS_MEET_DURATION", "")),
        realtime=env("PROMETHEUS_MEET_MODE", "transcribe").strip().lower() == "realtime",
        # The plugin's granted secret, inherited from the sidecar.
        realtime_api_key=env("OPENAI_API_KEY", ""),
        realtime_model=env("PROMETHEUS_MEET_REALTIME_MODEL", "gpt-realtime"),
        realtime_voice=env("PROMETHEUS_MEET_REALTIME_VOICE", "alloy"),
        realtime_instructions=env("PROMETHEUS_MEET_REALTIME_INSTRUCTIONS", ""),
        lobby_timeout=float(env("PROMETHEUS_MEET_LOBBY_TIMEOUT", "300")))


def _proxy() -> Optional[dict]:
    """The sidecar's egress proxy: the only way out of the sandbox."""
    server = os.environ.get("HTTPS_PROXY") or os.environ.get("HTTP_PROXY")
    return {"server": server} if server else None


# Realtime speech.

def _pcm_tail_loop(proc, pcm_path: Path, stop_flag: dict) -> None:
    """Feeds what Realtime appends to ``speaker.pcm`` into the audio pump."""
    try:
        with open(pcm_path, "rb") as f:
            while not stop_flag["stop"] and proc.poll() is None:
                chunk = f.read(65536)
                if not chunk:
                    time.sleep(0.05)
                    continue
                proc.stdin.write(chunk)
                proc.stdin.flush()
    except (OSError, ValueError):
        pass  # the pump exited: nothing left to stream to
    finally:
        _quiet(proc.stdin.close)


def _mac_audio_device_index(name: str) -> str:
    """ffmpeg's avfoundation index of device ``name`` (``"0"`` when absent)."""
    out = _quiet(subprocess.run, ["ffmpeg", "-f", "avfoundation", "-list_devices", "true", "-i", ""],
                 capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=10)
    for line in (out.stderr if out else "").splitlines():
        m = re.search(r"\[(\d+)\]\s+(.+)$", line)
        if m and m.group(2).strip().lower() == name.strip().lower():
            return m.group(1)
    return "0"


def _pump_command(bridge: dict, state: BotState) -> Optional[list[str]]:
    """The process that plays 24 kHz s16le mono PCM from stdin into the
    virtual microphone, or None (error recorded)."""
    target = bridge.get("write_target")
    if bridge.get("platform") == "linux":
        if not shutil.which("paplay"):
            state.set(error="paplay not found: install pulseaudio-utils for realtime on Linux")
            return None
        return ["paplay", "--raw", "--rate=24000", "--format=s16le", "--channels=1",
                f"--device={target or 'prometheus_meet_sink'}", "-"]
    if not shutil.which("ffmpeg"):
        state.set(error=_FFMPEG_MISSING)
        return None
    return ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "s16le", "-ar", "24000", "-ac", "1",
            "-i", "-", "-f", "audiotoolbox", "-audio_device_index",
            _mac_audio_device_index(target or "BlackHole 2ch"), "-"]


def _start_realtime_speaker(rt: dict, cfg: SimpleNamespace, stop_flag: dict, state: BotState) -> None:
    """The Realtime session, the say-queue speaker thread and the audio pump."""
    pcm_path, queue_path = cfg.out_dir / "speaker.pcm", cfg.out_dir / "say_queue.jsonl"
    pcm_path.write_bytes(b"")
    queue_path.touch()
    try:
        from realtime import RealtimeSession, RealtimeSpeaker
        session = RealtimeSession(api_key=cfg.realtime_api_key, model=cfg.realtime_model,
                                  voice=cfg.realtime_voice, instructions=cfg.realtime_instructions,
                                  audio_sink_path=pcm_path)
        session.connect()
    except Exception as exc:  # noqa: BLE001 - recorded; the call goes on transcribing
        state.set(error=f"realtime connect failed: {exc}")
        return
    rt["session"] = session
    speaker = RealtimeSpeaker(session, queue_path, cfg.out_dir / "say_processed.jsonl")

    def speak_loop():
        try:
            speaker.run_until_stopped(lambda: stop_flag["stop"])
        except Exception as exc:  # noqa: BLE001 - recorded in status.json
            state.set(error=f"realtime speaker crashed: {exc}")

    rt["speaker_thread"] = threading.Thread(target=speak_loop, name="meet-speaker", daemon=True)
    rt["speaker_thread"].start()
    command = _pump_command(rt["bridge_info"] or {}, state)
    if command is None:
        return
    try:
        rt["pcm_pump"] = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL)
    except OSError as exc:
        state.set(error=f"audio pump failed to start: {exc}")
        return
    rt["pcm_tail_thread"] = threading.Thread(target=_pcm_tail_loop, args=(rt["pcm_pump"], pcm_path, stop_flag),
                                             name="meet-pcm-tail", daemon=True)
    rt["pcm_tail_thread"].start()
    state.set(realtime_ready=True)


def _setup_realtime(rt: dict, api_key: str, state: BotState) -> None:
    """The virtual microphone; on any failure the call is transcribe-only."""
    if not api_key:
        state.set(error="realtime mode needs the OPENAI_API_KEY secret granted to google_meet: transcribing only")
        rt["enabled"] = False
        return
    try:
        from audio_bridge import AudioBridge
        rt["bridge"] = AudioBridge()
        rt["bridge_info"] = rt["bridge"].setup()
        state.set(realtime=True, realtime_device=rt["bridge_info"].get("device_name"))
    except Exception as exc:  # noqa: BLE001 - recorded; the call goes on transcribing
        state.set(error=f"audio bridge setup failed: {exc}: transcribing only")
        rt["enabled"] = False


def _teardown_realtime(rt: dict) -> None:
    if rt.get("pcm_pump"):
        _quiet(rt["pcm_pump"].terminate)
        _quiet(rt["pcm_pump"].wait, timeout=3)
    for key, method, kwargs in (("pcm_tail_thread", "join", {"timeout": 1.0}),
                                ("speaker_thread", "join", {"timeout": 5.0}), ("session", "close", {}),
                                ("bridge", "teardown", {})):
        if rt.get(key) is not None:
            _quiet(getattr(rt[key], method), **kwargs)


# The call.

def _join(page, cfg: SimpleNamespace, state: BotState, timeout: float = 30.0) -> None:
    """Fills the guest name and clicks 'Join now' or 'Ask to join' (the
    lobby); the buttons render late, so it polls for ``timeout``."""
    deadline = time.time() + timeout
    while True:
        name_box = _visible(page.locator('input[aria-label*="name" i]'))
        if name_box is not None:
            _quiet(name_box.fill, cfg.guest_name, timeout=2_000)
        for label in ("Join now", "Ask to join"):
            button = _visible(page.get_by_role("button", name=label, exact=False))
            if button is not None and _quiet(lambda b=button: (b.click(timeout=3_000), True)):
                if label == "Ask to join":
                    state.set(lobby_waiting=True)
                return
        if time.time() >= deadline:
            return
        time.sleep(0.5)


def _ensure_mic_on(page) -> str:
    """Unmutes once admitted (a muted bot cannot be heard in realtime)."""
    muted = _visible(page.locator('button[aria-label*="Turn on microphone" i]'))
    if muted is not None and _quiet(lambda: (muted.click(timeout=3_000), True)):
        return "unmuted_clicked"
    if _visible(page.locator('button[aria-label*="Turn off microphone" i]')) is not None:
        return "unmuted"
    return "unknown"


def _looks_like_human(speaker: str, guest_name: str) -> bool:
    """A caption by someone else (the bot's own speech is attributed to it)."""
    name = (speaker or "").strip().lower()
    return bool(name) and name not in {"unknown", "you", guest_name.strip().lower()}


def _admission(page, cfg: SimpleNamespace, state: BotState, now: float, lobby_deadline: float) -> bool:
    """Checks the lobby; False when the bot must leave."""
    if _probe(page, _ADMISSION_PROBE_JS):
        state.set(in_call=True, lobby_waiting=False, joined_at=now, mic_state=_ensure_mic_on(page))
    elif now > lobby_deadline:
        state.set(error=f"lobby timeout: not admitted within {int(cfg.lobby_timeout)}s", leave_reason="lobby_timeout")
        return False
    elif _probe(page, _DENIED_PROBE_JS):
        state.set(error="host denied admission", leave_reason="denied")
        return False
    return True


def _drain(page, cfg: SimpleNamespace, state: BotState, rt: dict, now: float) -> bool:
    """Moves queued captions to the transcript; False when the page is gone."""
    try:
        queued = page.evaluate("window.__prometheusMeetDrain && window.__prometheusMeetDrain()")
    except Exception:  # noqa: BLE001 - a reload or removal closes the page
        if page.is_closed():
            state.set(leave_reason="page_closed")
            return False
        return True
    for entry in (e for e in (queued if isinstance(queued, list) else ()) if isinstance(e, dict)):
        speaker = str(entry.get("speaker", ""))
        state.record_caption(speaker, str(entry.get("text", "")))
        # Barge-in: someone spoke while the bot may be talking.
        if rt["session"] is not None and _looks_like_human(speaker, cfg.guest_name) \
                and _quiet(rt["session"].cancel_response):
            state.set(last_barge_in_at=now)
    return True


def _drain_loop(page, cfg: SimpleNamespace, state: BotState, rt: dict, stop_flag: dict) -> None:
    """Runs until asked to leave, the duration ends, the lobby times out or
    refuses, or the page is lost."""
    deadline = time.time() + cfg.duration_s if cfg.duration_s else None
    lobby_deadline = time.time() + cfg.lobby_timeout
    last_check = 0.0
    while not stop_flag["stop"]:
        now = time.time()
        if (cfg.out_dir / LEAVE_REQUEST).exists():
            state.set(leave_reason="leave_requested")
            return
        if deadline and now > deadline:
            state.set(leave_reason="duration_expired")
            return
        if not state.in_call and now - last_check > 3.0:
            last_check = now
            if not _admission(page, cfg, state, now, lobby_deadline):
                return
        if not _drain(page, cfg, state, rt, now):
            return
        if rt["session"] is not None:
            state.set(audio_bytes_out=rt["session"].audio_bytes_out,
                      last_audio_out_at=rt["session"].last_audio_out_at)
        time.sleep(1.0)


_CONTEXT_ARGS = {
    "viewport": {"width": 1280, "height": 800},
    "user_agent": ("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
                   "Chrome/124.0.0.0 Safari/537.36"),
    "permissions": ["microphone", "camera"]}


def _launch(pw, cfg: SimpleNamespace, realtime: bool):
    args = ["--use-fake-ui-for-media-stream", "--disable-blink-features=AutomationControlled"]
    if not realtime:
        args.append("--use-fake-device-for-media-stream")  # a silent fake microphone
    options = {"headless": not cfg.headed, "args": args}
    if _proxy():
        options["proxy"] = _proxy()
    return pw.chromium.launch(**options)


def _in_call(pw, cfg: SimpleNamespace, state: BotState, rt: dict, stop_flag: dict) -> int:
    bridge = rt["bridge_info"] or {}
    if rt["enabled"] and bridge.get("platform") == "linux":
        # Chromium inherits it: its microphone is the virtual source.
        os.environ["PULSE_SOURCE"] = bridge.get("device_name", "")
    browser = _launch(pw, cfg, rt["enabled"])
    context_args = dict(_CONTEXT_ARGS)
    if cfg.auth_state and Path(cfg.auth_state).is_file():
        context_args["storage_state"] = cfg.auth_state
    context = browser.new_context(**context_args)
    page = context.new_page()
    try:
        page.goto(cfg.url, wait_until="domcontentloaded", timeout=30_000)
    except Exception as exc:  # noqa: BLE001 - recorded in status.json
        state.set(error=f"navigate failed: {exc}", exited=True)
        return 4
    _join(page, cfg, state)
    if _quiet(page.evaluate, _ENABLE_CAPTIONS_JS):
        state.set(captions_enabled_attempted=True)
    try:
        page.evaluate(_CAPTION_OBSERVER_JS)
    except Exception as exc:  # noqa: BLE001 - recorded in status.json
        state.set(error=f"caption observer install failed: {exc}")
    state.set(captioning=True, join_attempted_at=time.time())
    if rt["enabled"]:
        _start_realtime_speaker(rt, cfg, stop_flag, state)
    _drain_loop(page, cfg, state, rt, stop_flag)
    _quiet(page.evaluate, _LEAVE_CALL_JS)
    context.close()
    browser.close()
    return 0


def run_bot() -> int:
    cfg = config_from_env()
    if not is_meet_url(cfg.url):
        sys.stderr.write(f"google_meet bot: refusing a non-Meet URL: {cfg.url!r}\n")
        return 2
    if cfg.out_dir is None:
        sys.stderr.write("google_meet bot: PROMETHEUS_MEET_OUT_DIR is required\n")
        return 2
    state = BotState(cfg.out_dir, meeting_id(cfg.url), cfg.url)
    # SIGTERM sets a flag so the teardown below still finalizes the transcript.
    stop_flag = {"stop": False}
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda _sig, _frame: stop_flag.__setitem__("stop", True))
    rt = {"enabled": cfg.realtime, "bridge": None, "bridge_info": None, "session": None}
    if rt["enabled"]:
        _setup_realtime(rt, cfg.realtime_api_key, state)
    try:
        from playwright.sync_api import sync_playwright
    except ImportError as exc:
        state.set(error=f"playwright not installed: {exc}", exited=True)
        _teardown_realtime(rt)
        return 3
    try:
        with sync_playwright() as pw:
            code = _in_call(pw, cfg, state, rt, stop_flag)
    except Exception as exc:  # noqa: BLE001 - recorded in status.json
        state.set(error=f"unhandled: {exc}", exited=True)
        code = 1
    finally:
        _teardown_realtime(rt)
    state.set(in_call=False, captioning=False, exited=True)
    return code


def run_auth(auth_path: str, trigger: str) -> int:
    """Opens Google sign-in visibly and saves the session state to
    ``auth_path`` once ``trigger`` exists (written by ``... auth``)."""
    from playwright.sync_api import sync_playwright
    with sync_playwright() as pw:
        options = {"headless": False}
        if _proxy():
            options["proxy"] = _proxy()
        browser = pw.chromium.launch(**options)
        context = browser.new_context()
        page = context.new_page()
        page.goto("https://accounts.google.com/", wait_until="domcontentloaded")
        deadline = time.time() + 1800
        while not Path(trigger).exists():
            if time.time() > deadline:
                return 1
            time.sleep(0.5)
        context.storage_state(path=auth_path)
        browser.close()
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--auth":
        sys.exit(run_auth(sys.argv[2], sys.argv[3]))
    sys.exit(run_bot())
