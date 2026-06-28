-- FEAT-VACUUM: scoped tokens. A scoped token belongs to a user, lasts minutes rather than days, and
-- reaches only the plugins its scopes name, at no more than its holder has there and never as an
-- administrator. It is how a person hands an agent just enough of their access for one job.

ALTER TABLE core.api_tokens DROP CONSTRAINT api_tokens_kind_check;
ALTER TABLE core.api_tokens ADD CONSTRAINT api_tokens_kind_check
    CHECK (kind IN ('session', 'personal', 'service', 'plugin-registration', 'operator', 'scoped'));

ALTER TABLE core.api_tokens DROP CONSTRAINT api_tokens_check;
ALTER TABLE core.api_tokens ADD CONSTRAINT api_tokens_owner_check CHECK (
    (kind IN ('session', 'personal', 'scoped') AND user_id IS NOT NULL)
    OR (kind IN ('service', 'operator') AND service_account_id IS NOT NULL)
    OR (kind = 'plugin-registration' AND plugin_id IS NOT NULL)
);

ALTER TABLE core.api_tokens
    -- The permissions it is limited to, such as `plugin:kb:user:ro`; null for every other kind.
    ADD COLUMN scopes text[],
    -- The plugin that minted it for its holder, if one did; only that plugin revokes it for them.
    ADD COLUMN issued_by text,
    ADD CONSTRAINT api_tokens_scopes_check CHECK ((kind = 'scoped') = (scopes IS NOT NULL)),
    ADD CONSTRAINT api_tokens_scoped_expire CHECK (kind <> 'scoped' OR expires_at IS NOT NULL);
