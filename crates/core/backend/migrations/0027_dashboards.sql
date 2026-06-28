-- What each person chose to see on their dashboard, the page they land on: items as `plugin/id`, in
-- the order they are shown. No row means they have not chosen, and see the starter set.
CREATE TABLE core.dashboards (
    user_id     uuid PRIMARY KEY REFERENCES core.users (id) ON DELETE CASCADE,
    items       jsonb NOT NULL DEFAULT '[]'::jsonb,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
