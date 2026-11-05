"""Frames: a 4-byte big-endian length, then that many bytes of UTF-8 JSON."""

import json
import struct
import threading

from . import MAX_FRAME


class FrameTooLarge(Exception):
    """A frame above MAX_FRAME."""


class Wire:
    """One end of the channel: reads requests, writes answers, one writer at a time."""

    def __init__(self, reader, writer):
        self._reader = reader
        self._writer = writer
        self._lock = threading.Lock()

    def read(self):
        """The next message, or None at end of input."""
        head = self._exact(4)
        if head is None:
            return None
        (size,) = struct.unpack(">I", head)
        if size > MAX_FRAME:
            raise FrameTooLarge(f"frame_too_large: {size} bytes")
        body = self._exact(size)
        if body is None:
            return None
        return json.loads(body.decode("utf-8"))

    def _exact(self, size):
        chunks = []
        left = size
        while left:
            chunk = self._reader.read(left)
            if not chunk:
                return None
            chunks.append(chunk)
            left -= len(chunk)
        return b"".join(chunks)

    def write(self, message):
        """Sends one message; a body above MAX_FRAME raises FrameTooLarge."""
        body = json.dumps(message, ensure_ascii=False, default=str).encode("utf-8")
        if len(body) > MAX_FRAME:
            raise FrameTooLarge(f"frame_too_large: {len(body)} bytes exceeds {MAX_FRAME}")
        with self._lock:
            self._writer.write(struct.pack(">I", len(body)) + body)
            self._writer.flush()
