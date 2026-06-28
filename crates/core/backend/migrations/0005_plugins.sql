-- What registration adds to the plugin registry from 0002. `plugin_versions` remembers every
-- version ever seen so one cannot come back with a different binary, and `plugin_permissions` is
-- what the RBAC plugin reads to know what a plugin can have granted to it.

ALTER TABLE core.plugins
    ADD COLUMN version        text,
    ADD COLUMN classification text,
    ADD COLUMN state          text
        CHECK (state IN ('loading', 'running', 'cancelled', 'unloading', 'error')),
    ADD COLUMN address        text,
    ADD COLUMN binary_sha256  text,
    ADD COLUMN manifest       jsonb NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN error          text,
    ADD COLUMN last_seen_at   timestamptz,
    ADD COLUMN updated_at     timestamptz NOT NULL DEFAULT now();

CREATE TABLE core.plugin_versions (
    plugin        text NOT NULL REFERENCES core.plugins (id) ON DELETE CASCADE,
    version       text NOT NULL,
    binary_sha256 text NOT NULL,
    manifest      jsonb NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plugin, version)
);

-- `name` is empty for the two permissions every plugin has by existing, and set for the ones its
-- manifest declares.
CREATE TABLE core.plugin_permissions (
    plugin     text NOT NULL REFERENCES core.plugins (id) ON DELETE CASCADE,
    kind       text NOT NULL
        CHECK (kind IN ('user', 'service', 'pluginuser', 'pluginservice')),
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plugin, kind, name),
    CONSTRAINT plugin_permissions_named CHECK (
        (kind IN ('user', 'service') AND name = '') OR
        (kind IN ('pluginuser', 'pluginservice') AND name <> '')
    )
);

-- Migrations already run in the plugin's own schema. The hash is kept so an author who edits a
-- migration that has already run is told, rather than silently getting a different schema.
CREATE TABLE core.plugin_migrations (
    plugin text NOT NULL REFERENCES core.plugins (id) ON DELETE CASCADE,
    name   text NOT NULL,
    sha256 text NOT NULL,
    ran_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plugin, name)
);

DROP VIEW core_v1.plugins;

CREATE VIEW core_v1.plugins AS
SELECT id, display_name, version, classification, state, registered_at, last_seen_at
FROM core.plugins;

CREATE VIEW core_v1.plugin_permissions AS
SELECT plugin, kind, name
FROM core.plugin_permissions;
