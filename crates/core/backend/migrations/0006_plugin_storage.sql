-- `backend.state()`: each plugin's durable key/value store for checkpoints and state handover.
-- The record of which migrations a plugin has had moves into its own schema, because they now run
-- as the plugin's own role and the record must commit in the same transaction as the migration.

CREATE TABLE core.plugin_state (
    plugin     text NOT NULL REFERENCES core.plugins (id) ON DELETE CASCADE,
    key        text NOT NULL,
    value      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plugin, key)
);

DROP TABLE core.plugin_migrations;
