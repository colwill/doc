-- FEAT-TEAMS: a team's lead, and the positions people hold in it. An organisation defines its
-- positions (Software Engineer, QA Engineer, ...) with their responsibilities; every team inherits
-- them through its parents and can change, hide or add to them without touching anyone else's.

-- Everyone a team has is in core.team_members, so its lead is one of them: someone who leaves the
-- team stops leading it.
ALTER TABLE core.team_members ADD COLUMN position text;

ALTER TABLE core.teams
    ADD COLUMN lead_id uuid,
    ADD CONSTRAINT teams_lead_is_a_member FOREIGN KEY (id, lead_id)
        REFERENCES core.team_members (team_id, user_id) ON DELETE SET NULL (lead_id);

CREATE TABLE core.positions (
    id               uuid PRIMARY KEY,
    organisation_id  uuid NOT NULL REFERENCES core.organisations (id) ON DELETE CASCADE,
    -- What teams and members name it by; it never changes, so a title can.
    name             text NOT NULL,
    title            text NOT NULL,
    description      text NOT NULL DEFAULT '',
    responsibilities text[] NOT NULL DEFAULT '{}',
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organisation_id, name)
);

-- One team's change to a position it inherits, or a position of its own when nothing above it has
-- one of that name. A null keeps what comes from above; its sub-teams inherit the result.
CREATE TABLE core.team_positions (
    team_id     uuid NOT NULL REFERENCES core.teams (id) ON DELETE CASCADE,
    name        text NOT NULL,
    title       text,
    description text,
    -- Responsibilities this team adds to the ones it inherits, and the inherited ones it drops.
    added       text[] NOT NULL DEFAULT '{}',
    removed     text[] NOT NULL DEFAULT '{}',
    -- True hides an inherited position here and below, false shows one hidden above again.
    hidden      boolean,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, name)
);

DROP VIEW core_v1.teams;
CREATE VIEW core_v1.teams AS
SELECT id, organisation_id, parent_id, name, title, description, email, is_default, provider,
       external_id, lead_id, created_at
FROM core.teams;

DROP VIEW core_v1.team_members;
CREATE VIEW core_v1.team_members AS
SELECT team_id, user_id, source, provider, position, created_at
FROM core.team_members;

CREATE VIEW core_v1.positions AS
SELECT id, organisation_id, name, title, description, responsibilities, created_at
FROM core.positions;

CREATE VIEW core_v1.team_positions AS
SELECT team_id, name, title, description, added, removed, hidden, created_at
FROM core.team_positions;
