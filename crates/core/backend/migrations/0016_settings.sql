-- What an administrator set for the whole platform, such as the name shown beside the logo. One
-- row, as the navigation has, so every frontend reads the same settings.
CREATE TABLE core.settings (
    id         boolean PRIMARY KEY DEFAULT true CHECK (id),
    settings   jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
