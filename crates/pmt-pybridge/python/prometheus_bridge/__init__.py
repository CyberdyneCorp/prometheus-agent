"""The Prometheus python bridge: hosts one Hermes-style plugin per process
and speaks length-prefixed JSON-RPC 2.0 with the Prometheus host over
stdin/stdout (stderr carries logs)."""

PROTOCOL_VERSION = 1
MAX_FRAME = 16 * 1024 * 1024
