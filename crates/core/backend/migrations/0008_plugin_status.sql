-- What the plugin probe keeps (T24): each plugin's state as the registry last had it, and every
-- state it has been in, so a hot reload or a crash can be read back afterwards.

CREATE TABLE core.plugin_status (
    plugin         text PRIMARY KEY,
    version        text NOT NULL,
    classification text NOT NULL,
    instance       uuid NOT NULL,
    -- No state means the plugin has left the registry, as in core.plugins.
    state          text CHECK (state IN ('loading', 'running', 'cancelled', 'unloading', 'error')),
    error          text,
    since          timestamptz NOT NULL,
    at             timestamptz NOT NULL,
    registered_at  timestamptz NOT NULL,
    last_error     text,
    last_error_at  timestamptz,
    checked_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE core.plugin_status_history (
    id       bigserial PRIMARY KEY,
    plugin   text NOT NULL,
    version  text NOT NULL,
    instance uuid NOT NULL,
    state    text NOT NULL
        CHECK (state IN ('loading', 'running', 'cancelled', 'unloading', 'error', 'removed')),
    error    text,
    at       timestamptz NOT NULL,
    source   text NOT NULL CHECK (source IN ('event', 'registry')),
    -- An event delivered twice, or a change seen in both an event and the registry, is kept once.
    CONSTRAINT plugin_status_history_once UNIQUE (plugin, instance, state, at)
);

CREATE INDEX plugin_status_history_plugin_idx ON core.plugin_status_history (plugin, at DESC);
CREATE INDEX plugin_status_history_at_idx ON core.plugin_status_history (at);

CREATE VIEW core_v1.plugin_status AS
SELECT plugin, version, classification, state, error, since, registered_at, last_error,
       last_error_at, checked_at
FROM core.plugin_status;

CREATE VIEW core_v1.plugin_status_history AS
SELECT plugin, version, state, error, at
FROM core.plugin_status_history;
