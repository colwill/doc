-- The service account a plugin with the `service-account` capability acts as when it asks another
-- plugin's api/ as itself (FEAT-AGENT: runbooks). Core makes it when the plugin first registers and
-- never claims one somebody else made; it holds whatever administrators grant it in RBAC, and no
-- token is ever issued for it, so only the plugin acts as it.
CREATE TABLE core.plugin_service_accounts (
    plugin      text PRIMARY KEY,
    account_id  uuid NOT NULL UNIQUE REFERENCES core.service_accounts (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now()
);
