-- The plugins somebody has turned off. A plugin that is off stays registered but is held
-- `cancelled`, takes no runs or events, and is offered to nobody: no navigation, panels or pages.
-- It stays off across restarts and new versions until somebody turns it on, which deletes its row.
CREATE TABLE core.plugin_switches (
    plugin      text PRIMARY KEY,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text
);
