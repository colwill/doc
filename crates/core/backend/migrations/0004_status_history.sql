-- Every status check, kept so the dashboard can chart how latency and replication lag move.
-- A cron task prunes it; the current answer for each component lives in the Cache Bus, not here.

CREATE TABLE core.status_history (
    id          bigserial PRIMARY KEY,
    at          timestamptz NOT NULL DEFAULT now(),
    kind        text NOT NULL,
    name        text NOT NULL,
    state       text NOT NULL CHECK (state IN ('up', 'degraded', 'down', 'unknown')),
    detail      text,
    latency_ms  integer,
    nodes       jsonb NOT NULL DEFAULT '[]'::jsonb
);

CREATE INDEX status_history_name_idx ON core.status_history (name, at DESC);
CREATE INDEX status_history_at_idx ON core.status_history (at);

CREATE VIEW core_v1.status_history AS
SELECT at, kind, name, state, detail, latency_ms, nodes
FROM core.status_history;
