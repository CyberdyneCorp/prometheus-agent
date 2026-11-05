-- Schema v19 (spec: plugin-system, MCP tool costs and spend limits).
--
-- `mcp_spend`: what one conversation spent on an MCP server's costed
-- tools, in the server's `cost_unit`. Keyed by the conversation's root
-- session, so the total follows the conversation across compression
-- continuations and branches.

CREATE TABLE mcp_spend (
    root_session_id TEXT NOT NULL,
    server          TEXT NOT NULL,
    amount          REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (root_session_id, server)
) STRICT;
