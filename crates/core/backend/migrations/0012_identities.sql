-- ADR-0005: a user is a person, and an identity is an account they sign in with or link. A user can
-- have any number of identities, one per provider, and can exist before they sign in with none.
CREATE TABLE core.identities (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES core.users (id) ON DELETE CASCADE,
    provider     text NOT NULL,
    -- The provider's immutable ID for the account, such as GitHub's numeric user ID. A login can be
    -- renamed and then taken by someone else, so it never identifies anyone.
    external_id  text NOT NULL,
    login        text NOT NULL,
    -- How it came to be attached: 'sign-in', 'link', 'admin' or 'provider'.
    source       text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    UNIQUE (provider, external_id),
    UNIQUE (user_id, provider)
);

-- Until now each user was exactly the account they signed in with.
INSERT INTO core.identities (id, user_id, provider, external_id, login, source, created_at, last_used_at)
SELECT gen_random_uuid(), id, provider, external_id, login, 'sign-in', created_at, last_signed_in_at
FROM core.users;

-- The user keeps `login` as the name they go by. Dropping the columns drops their constraints.
DROP VIEW core_v1.users;
ALTER TABLE core.users DROP COLUMN provider, DROP COLUMN external_id;

CREATE VIEW core_v1.users AS
SELECT id, login, name, email, disabled, first_signed_in_at, last_signed_in_at, created_at
FROM core.users;

CREATE VIEW core_v1.identities AS
SELECT id, user_id, provider, external_id, login, source, created_at, last_used_at
FROM core.identities;
