#!/usr/bin/env python3
"""A stand-in for `lightpanda serve --host H --port P`: a DevTools
WebSocket server answering every command; `Browser.getVersion` also
reports the command line it was started with."""

import base64
import hashlib
import json
import socket
import struct
import sys
import threading

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def option(name):
    return sys.argv[sys.argv.index(name) + 1]


def read_exact(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def read_frame(conn):
    head = read_exact(conn, 2)
    opcode = head[0] & 0x0F
    length = head[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", read_exact(conn, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", read_exact(conn, 8))[0]
    mask = read_exact(conn, 4) if head[1] & 0x80 else b"\0\0\0\0"
    payload = bytearray(read_exact(conn, length))
    for i in range(length):
        payload[i] ^= mask[i % 4]
    return opcode, bytes(payload)


def send_text(conn, text):
    data = text.encode()
    if len(data) < 126:
        head = struct.pack(">BB", 0x81, len(data))
    elif len(data) < 65536:
        head = struct.pack(">BBH", 0x81, 126, len(data))
    else:
        head = struct.pack(">BBQ", 0x81, 127, len(data))
    conn.sendall(head + data)


def serve(conn):
    request = b""
    while b"\r\n\r\n" not in request:
        chunk = conn.recv(4096)
        if not chunk:
            return
        request += chunk
    key = ""
    for line in request.decode("latin-1").split("\r\n"):
        if line.lower().startswith("sec-websocket-key:"):
            key = line.split(":", 1)[1].strip()
    accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
    conn.sendall(
        (
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n"
        ).encode()
    )
    while True:
        try:
            opcode, payload = read_frame(conn)
        except (EOFError, OSError):
            return
        if opcode == 8:
            return
        if opcode != 1:
            continue
        message = json.loads(payload)
        result = {}
        if message.get("method") == "Browser.getVersion":
            result = {"product": "Lightpanda/fake", "argv": sys.argv[1:]}
        send_text(conn, json.dumps({"id": message.get("id"), "result": result}))


def main():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((option("--host"), int(option("--port"))))
    server.listen(8)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
