-- How an administrator arranged the navigation: one layout for the whole platform, which the
-- frontend lays over the pages each person may open.
CREATE TABLE core.navigation (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    layout jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
