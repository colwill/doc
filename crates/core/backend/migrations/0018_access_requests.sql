-- A plugin asking an administrator to be added to another plugin's requestable list setting, such
-- as github's `archive-plugins`. One row per plugin, target and setting: asking again only says
-- where it stands, so a plugin refused on every call raises one request, not one each time.
CREATE TABLE core.access_requests (
    id          uuid PRIMARY KEY,
    requester   text NOT NULL,
    target      text NOT NULL,
    setting     text NOT NULL,
    reason      text NOT NULL DEFAULT '',
    state       text NOT NULL CHECK (state IN ('pending', 'approved', 'denied')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    decided_at  timestamptz,
    decided_by  text,
    UNIQUE (requester, target, setting)
);

CREATE INDEX access_requests_target ON core.access_requests (target, created_at DESC);
