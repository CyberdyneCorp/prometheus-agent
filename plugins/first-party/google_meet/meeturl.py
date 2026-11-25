"""Which URLs the plugin joins: Google Meet only. A leaf module (no sibling
imports): the sidecar imports it as part of the plugin package and the bot
process imports it by file."""

from __future__ import annotations

import re
import time

# A three-segment meeting code, a lookup URL or /new; anything else is refused.
MEET_URL_RE = re.compile(
    r"^https://meet\.google\.com/([a-z0-9]{3,}-[a-z0-9]{3,}-[a-z0-9]{3,}|lookup/[^/?#]+|new)"
    r"(?:[/?#].*)?$")

REFUSAL = "only meet.google.com URLs are accepted"


def is_meet_url(url: object) -> bool:
    """True for a ``https://meet.google.com/...`` meeting URL."""
    return isinstance(url, str) and bool(MEET_URL_RE.match(url.strip()))


def meeting_id(url: str) -> str:
    """The meeting code, or a timestamped id for ``/lookup/...`` and ``/new``."""
    m = re.search(r"meet\.google\.com/([a-z0-9]{3,}-[a-z0-9]{3,}-[a-z0-9]{3,})", url or "")
    return m.group(1) if m else f"meet-{int(time.time())}"
