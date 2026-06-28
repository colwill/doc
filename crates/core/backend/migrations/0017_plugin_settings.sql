-- Each plugin's settings and features, as an administrator set them (ADR-0007). A value is either
-- plain JSON or, for a secret, ciphertext with the nonce and the ID of the key it was encrypted
-- under: the database only ever holds the encrypted form, and the key lives in the secrets volume.
CREATE TABLE core.plugin_settings (
    plugin      text NOT NULL,
    key         text NOT NULL,
    value       jsonb,
    secret      bytea,
    nonce       bytea,
    key_id      text,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text,
    PRIMARY KEY (plugin, key),
    -- A row is one or the other, never both and never neither.
    CONSTRAINT plugin_settings_one_form CHECK (
        (value IS NOT NULL AND secret IS NULL AND nonce IS NULL AND key_id IS NULL)
        OR (value IS NULL AND secret IS NOT NULL AND nonce IS NOT NULL AND key_id IS NOT NULL)
    )
);

-- The switches on a plugin's Features tab. A feature with no row is at the default its manifest
-- declares, so turning nothing on and off leaves no rows at all.
CREATE TABLE core.plugin_features (
    plugin      text NOT NULL,
    name        text NOT NULL,
    enabled     boolean NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text,
    PRIMARY KEY (plugin, name)
);
