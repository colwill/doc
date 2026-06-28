-- ADR-0005 as settled: a person belongs to exactly one organisation and signs in with the identity
-- plugins it chose, each of which serves one organisation. Each identity keeps the profile its
-- provider last reported.

-- There is always an organisation for people to belong to.
INSERT INTO core.organisations (id, name, title, description)
SELECT gen_random_uuid(), 'default', 'Default organisation',
       'Made at first start to hold the default teams. Rename it after your own.'
WHERE NOT EXISTS (SELECT 1 FROM core.organisations);

ALTER TABLE core.users
    ADD COLUMN organisation_id uuid REFERENCES core.organisations (id) ON DELETE RESTRICT,
    ADD COLUMN first_name text,
    ADD COLUMN surname text;

-- Everyone so far joins the organisation that holds the default teams, or else the oldest.
CREATE TEMPORARY TABLE joined ON COMMIT DROP AS
SELECT o.id FROM core.organisations o
ORDER BY EXISTS (SELECT 1 FROM core.teams t WHERE t.organisation_id = o.id AND t.is_default) DESC,
         o.created_at, o.id
LIMIT 1;

UPDATE core.users SET organisation_id = (SELECT id FROM joined);
ALTER TABLE core.users ALTER COLUMN organisation_id SET NOT NULL;
CREATE INDEX users_organisation_idx ON core.users (organisation_id);

ALTER TABLE core.identities
    ADD COLUMN name text,
    ADD COLUMN email text,
    ADD COLUMN first_name text,
    ADD COLUMN surname text,
    ADD COLUMN reported_at timestamptz;

-- The identity plugins each organisation's people sign in with. A provider serves one organisation.
CREATE TABLE core.organisation_providers (
    provider        text PRIMARY KEY,
    organisation_id uuid NOT NULL REFERENCES core.organisations (id) ON DELETE CASCADE,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX organisation_providers_organisation_idx
    ON core.organisation_providers (organisation_id);

-- Whatever people have signed in with keeps working, and so does the DOC accounts plugin, `local`.
INSERT INTO core.organisation_providers (provider, organisation_id)
SELECT seen.provider, joined.id
FROM (SELECT DISTINCT provider FROM core.identities UNION SELECT 'local') AS seen (provider), joined;

DROP VIEW core_v1.users;
CREATE VIEW core_v1.users AS
SELECT id, login, name, email, first_name, surname, organisation_id, disabled,
       first_signed_in_at, last_signed_in_at, created_at
FROM core.users;

DROP VIEW core_v1.identities;
CREATE VIEW core_v1.identities AS
SELECT id, user_id, provider, external_id, login, source, name, email, first_name, surname,
       reported_at, created_at, last_used_at
FROM core.identities;

CREATE VIEW core_v1.organisation_providers AS
SELECT provider, organisation_id, created_at
FROM core.organisation_providers;
