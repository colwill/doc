-- What a plugin's last `unload` handed over, kept for whichever version loads next, so a hot reload
-- or a restart of the backend itself still carries it (T23).

ALTER TABLE core.plugins
    ADD COLUMN handover    jsonb,
    ADD COLUMN handover_at timestamptz;
