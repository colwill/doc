-- ADR-0004: organisations and teams belong to core. Every team is in exactly one organisation, and
-- can sit inside another team of the same organisation. Members are users; service accounts never are.
CREATE TABLE core.organisations (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    title       text NOT NULL,
    description text NOT NULL DEFAULT '',
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE core.teams (
    id              uuid PRIMARY KEY,
    organisation_id uuid NOT NULL REFERENCES core.organisations (id) ON DELETE RESTRICT,
    -- A sub-team's parent, in the same organisation. Core refuses a team below itself.
    parent_id       uuid REFERENCES core.teams (id) ON DELETE RESTRICT,
    name            text NOT NULL,
    title           text NOT NULL,
    description     text NOT NULL DEFAULT '',
    -- Everyone is placed in every default team as they are made.
    is_default      boolean NOT NULL DEFAULT false,
    -- The plugin that provides the team and its key for it there; both null for a team made in DOC.
    provider        text,
    external_id     text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organisation_id, name),
    UNIQUE (provider, external_id),
    CHECK ((provider IS NULL) = (external_id IS NULL)),
    CHECK (parent_id IS DISTINCT FROM id)
);

CREATE INDEX teams_parent_idx ON core.teams (parent_id) WHERE parent_id IS NOT NULL;

CREATE TABLE core.team_members (
    team_id    uuid NOT NULL REFERENCES core.teams (id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES core.users (id) ON DELETE CASCADE,
    -- 'admin' when added by hand, 'default' when placed in a default team, or 'provider'.
    source     text NOT NULL CHECK (source IN ('admin', 'default', 'provider')),
    provider   text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, user_id),
    CHECK ((source = 'provider') = (provider IS NOT NULL))
);

CREATE INDEX team_members_user_idx ON core.team_members (user_id);

-- A service account is owned by a user or by a team, whose members manage it.
ALTER TABLE core.service_accounts
    ADD COLUMN owner_team_id uuid REFERENCES core.teams (id) ON DELETE RESTRICT,
    ADD CONSTRAINT service_accounts_one_owner CHECK (owner_id IS NULL OR owner_team_id IS NULL);

-- The default teams, made once at first start, in an organisation made to hold them.
WITH organisation AS (
    INSERT INTO core.organisations (id, name, title, description)
    VALUES (gen_random_uuid(), 'default', 'Default organisation',
            'Made at first start to hold the default teams. Rename it after your own.')
    RETURNING id
)
INSERT INTO core.teams (id, organisation_id, name, title, description, is_default)
SELECT gen_random_uuid(), organisation.id, team.name, team.title,
       'A default team, made at first start. Everyone is placed in it as they arrive.', true
FROM organisation, (VALUES ('leadership', 'Leadership'), ('product', 'Product')) AS team (name, title);

-- Everyone already here is placed in them, as everyone after will be.
INSERT INTO core.team_members (team_id, user_id, source)
SELECT teams.id, users.id, 'default' FROM core.teams CROSS JOIN core.users WHERE teams.is_default;

DROP VIEW core_v1.service_accounts;
CREATE VIEW core_v1.service_accounts AS
SELECT id, name, description, owner_id, owner_team_id, disabled, created_at
FROM core.service_accounts;

CREATE VIEW core_v1.organisations AS
SELECT id, name, title, description, created_at
FROM core.organisations;

CREATE VIEW core_v1.teams AS
SELECT id, organisation_id, parent_id, name, title, description, is_default, provider, external_id,
       created_at
FROM core.teams;

CREATE VIEW core_v1.team_members AS
SELECT team_id, user_id, source, provider, created_at
FROM core.team_members;
