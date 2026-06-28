-- A task queued while another runs joins that run's chain, named by its first task, so whoever
-- started it can wait for all of it.
ALTER TABLE core.tasks ADD COLUMN chain uuid;

CREATE INDEX tasks_chain_idx ON core.tasks (chain) WHERE chain IS NOT NULL;
