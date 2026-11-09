-- Schema v34 (spec: voice-mode, Gateway per-chat voice reply command).
--
-- `gateway_chat_prefs.voice`: the chat's `/voice` choice overriding
-- `gateway.voice.reply_mode` for that chat (`mirror` for `/voice on`,
-- `always` for `/voice tts`, `off`); NULL follows the configuration.
ALTER TABLE gateway_chat_prefs ADD COLUMN voice TEXT;
