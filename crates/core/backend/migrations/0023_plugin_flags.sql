-- The flag a plugin follows, if any. While it follows one the platform turns the plugin on and off
-- as the flag says, reading it from the flags plugin as the service `doc`; a plugin that follows
-- none is turned on and off by hand.
CREATE TABLE core.plugin_flags (
    plugin      text PRIMARY KEY,
    flag        text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text
);
