-- A team's contact address (ADR-0004 left it open; the Catalogue needs it in T69, where a team
-- stops being kept there as well and becomes one record here).

ALTER TABLE core.teams ADD COLUMN email text NOT NULL DEFAULT '';
