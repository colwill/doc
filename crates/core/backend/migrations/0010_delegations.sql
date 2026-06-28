-- A plugin's standing leave to start work as someone, such as an automation running as its owner.
-- It is given during a call that person made, and revoking it keeps the row for the record.
CREATE TABLE core.delegations (
    id uuid PRIMARY KEY,
    plugin text NOT NULL,
    principal text NOT NULL,
    purpose text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz
);

CREATE INDEX delegations_principal_idx ON core.delegations (principal) WHERE revoked_at IS NULL;
