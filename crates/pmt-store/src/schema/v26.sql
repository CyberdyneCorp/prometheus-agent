-- Schema v26 (spec: scheduled-jobs, Push notification flag, cron threads
-- and delivery confirmation).
--
-- `gateway_outbox.notify` says how an entry notifies its recipient: NULL
-- leaves it to the adapter (its quiet notification mode, where it has
-- one), 1 sends it as a final, notifying message whatever that mode says,
-- 0 sends it silently where the platform can.

ALTER TABLE gateway_outbox ADD COLUMN notify INTEGER CHECK (notify IN (0, 1));
