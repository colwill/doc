-- FEAT-PEOPLE: the email domains an organisation approves. An administrator or a team lead can add
-- somebody by an address in one of them, and they can then sign in with a DOC password before
-- the organisation's SSO is set up. A domain belongs to one organisation, as a provider does.
CREATE TABLE core.organisation_domains (
    domain          text PRIMARY KEY,
    organisation_id uuid NOT NULL REFERENCES core.organisations (id) ON DELETE CASCADE,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX organisation_domains_organisation_idx ON core.organisation_domains (organisation_id);
