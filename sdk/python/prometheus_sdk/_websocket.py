"""A minimal RFC 6455 WebSocket client (text frames, ping, close) over a
plain TCP socket: the daemon's RPC listener is loopback ``ws://`` only, and
the SDK depends on nothing outside the standard library."""

from __future__ import annotations

import base64
import hashlib
import os
import socket
import struct
import threading
from typing import Optional
from urllib.parse import urlparse

_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WebSocket:
    """One client connection."""

    def __init__(self, url: str, protocols: str) -> None:
        parsed = urlparse(url)
        if parsed.scheme != "ws" or not parsed.hostname or not parsed.port:
            raise ConnectionError(f"not a loopback ws:// URL: {url}")
        self._sock = socket.create_connection((parsed.hostname, parsed.port))
        self._send_lock = threading.Lock()
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {parsed.path or '/'} HTTP/1.1\r\nHost: {parsed.hostname}:{parsed.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Protocol: {protocols}\r\n\r\n"
        )
        self._sock.sendall(request.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self._sock.recv(1)
            if not chunk:
                raise ConnectionError("the daemon closed the connection during the handshake")
            head += chunk
        lines = head.decode("latin-1").split("\r\n")
        if " 101 " not in lines[0] + " ":
            raise ConnectionError(f"WebSocket upgrade refused: {lines[0]}")
        accept = base64.b64encode(hashlib.sha1((key + _GUID).encode()).digest()).decode()
        headers = {k.strip().lower(): v.strip() for k, _, v in (line.partition(":") for line in lines[1:] if line)}
        if headers.get("sec-websocket-accept") != accept:
            raise ConnectionError("WebSocket upgrade answered a wrong accept key")

    def _frame(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        size = len(payload)
        if size < 126:
            header = struct.pack("!BB", 0x80 | opcode, 0x80 | size)
        elif size < 1 << 16:
            header = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, size)
        else:
            header = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, size)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        with self._send_lock:
            self._sock.sendall(header + mask + masked)

    def send(self, text: str) -> None:
        self._frame(0x1, text.encode())

    def _exact(self, count: int) -> bytes:
        data = b""
        while len(data) < count:
            chunk = self._sock.recv(count - len(data))
            if not chunk:
                raise ConnectionError("connection closed")
            data += chunk
        return data

    def recv(self) -> Optional[str]:
        """The next text message; ``None`` once the connection closed."""
        message = b""
        try:
            while True:
                first, second = self._exact(2)
                opcode, size = first & 0x0F, second & 0x7F
                if size == 126:
                    size = struct.unpack("!H", self._exact(2))[0]
                elif size == 127:
                    size = struct.unpack("!Q", self._exact(8))[0]
                payload = self._exact(size)
                if opcode == 0x8:
                    return None
                if opcode == 0x9:
                    self._frame(0xA, payload)
                    continue
                if opcode in (0x1, 0x0):
                    message += payload
                    if first & 0x80:
                        return message.decode()
        except (ConnectionError, OSError):
            return None

    def close(self) -> None:
        try:
            self._frame(0x8, struct.pack("!H", 1000))
        except OSError:
            pass
        self._sock.close()
