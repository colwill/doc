-- Identity: who can call the platform, the tokens they use, the plugin registry and the audit log.
-- Plugins never see this schema; they read the core_v1 views below, which leave credentials out.

CREATE TABLE core.users (
    id                 uuid PRIMARY KEY,
    provider           text NOT NULL,
    external_id        text NOT NULL,
    login              text NOT NULL,
    name               text,
    email              text,
    disabled           boolean NOT NULL DEFAULT false,
    first_signed_in_at timestamptz,
    last_signed_in_at  timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider, external_id),
    UNIQUE (provider, login)
);

CREATE TABLE core.service_accounts (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    description text,
    -- Null means the platform owns it, which is how the operator account is created.
    owner_id    uuid REFERENCES core.users (id) ON DELETE RESTRICT,
    disabled    boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE core.api_tokens (
    id                 uuid PRIMARY KEY,
    kind               text NOT NULL
        CHECK (kind IN ('session', 'personal', 'service', 'plugin-registration', 'operator')),
    -- SHA-256 of the token; the token itself is shown once and never stored.
    token_hash         bytea NOT NULL UNIQUE,
    name               text,
    user_id            uuid REFERENCES core.users (id) ON DELETE CASCADE,
    service_account_id uuid REFERENCES core.service_accounts (id) ON DELETE CASCADE,
    plugin_id          text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    last_used_at       timestamptz,
    expires_at         timestamptz,
    revoked_at         timestamptz,
    CHECK (
        (kind IN ('session', 'personal') AND user_id IS NOT NULL)
        OR (kind IN ('service', 'operator') AND service_account_id IS NOT NULL)
        OR (kind = 'plugin-registration' AND plugin_id IS NOT NULL)
    )
);

CREATE INDEX api_tokens_user_idx ON core.api_tokens (user_id) WHERE user_id IS NOT NULL;
CREATE INDEX api_tokens_service_account_idx ON core.api_tokens (service_account_id)
    WHERE service_account_id IS NOT NULL;
CREATE INDEX api_tokens_plugin_idx ON core.api_tokens (plugin_id) WHERE plugin_id IS NOT NULL;

CREATE TABLE core.plugins (
    id            text PRIMARY KEY,
    display_name  text,
    registered_at timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE core.audit_log (
    id          uuid PRIMARY KEY,
    at          timestamptz NOT NULL DEFAULT now(),
    actor_kind  text NOT NULL,
    actor_id    text,
    actor_label text,
    action      text NOT NULL,
    subject     text,
    detail      jsonb NOT NULL DEFAULT '{}'::jsonb,
    request_id  text
);

CREATE INDEX audit_log_at_idx ON core.audit_log (at DESC);
CREATE INDEX audit_log_action_idx ON core.audit_log (action, at DESC);

-- What plugins may read: identity without credentials.
CREATE VIEW core_v1.users AS
SELECT id, provider, login, name, email, disabled, first_signed_in_at, last_signed_in_at, created_at
FROM core.users;

CREATE VIEW core_v1.service_accounts AS
SELECT id, name, description, owner_id, disabled, created_at
FROM core.service_accounts;

CREATE VIEW core_v1.plugins AS
SELECT id, display_name, registered_at
FROM core.plugins;
