-- How far administrators have got with setting DOC up the first time: the steps done, and when it
-- was finished or put aside. One row, as the settings have, so every frontend offers the same.
CREATE TABLE core.setup (
    id         boolean PRIMARY KEY DEFAULT true CHECK (id),
    setup      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
