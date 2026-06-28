-- The fabric, kept in the database, for a deployment that runs without the Raft clusters
-- (`fabric.mode = "memory"`). Every process that has the database then shares one Event Bus, one
-- Service Bus and one Cache Bus, so the workers' cron reaches the backend's task pools and the
-- backend's status page sees what the probes found. In cluster mode none of this is touched.
--
-- Each envelope is stored whole, as the JSON it already serialises to, and the columns beside it
-- are only what has to be indexed or compared.

-- Event Bus ---------------------------------------------------------------------------------

CREATE TABLE core.fabric_events (
    sequence        bigserial PRIMARY KEY,
    id              uuid NOT NULL UNIQUE,
    topic           text NOT NULL,
    idempotency_key text,
    event           jsonb NOT NULL,
    published_at    timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX fabric_events_idempotency
    ON core.fabric_events (idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX fabric_events_topic ON core.fabric_events (topic, sequence);
CREATE INDEX fabric_events_published_at ON core.fabric_events (published_at);

-- How much history each topic pattern keeps, as the backend registers it.
CREATE TABLE core.fabric_topic_specs (
    filter      text PRIMARY KEY,
    max_events  bigint NOT NULL,
    max_age_s   bigint NOT NULL
);

-- How many have ever been published on a topic, which retention does not tell you.
CREATE TABLE core.fabric_topic_counts (
    topic       text PRIMARY KEY,
    published   bigint NOT NULL DEFAULT 0
);

-- A consumer group shares one cursor: each event goes to one member of the group.
CREATE TABLE core.fabric_event_groups (
    name            text PRIMARY KEY,
    filter          text NOT NULL,
    at_sequence     bigint NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- An event handed to a member of a group and not yet acknowledged.
CREATE TABLE core.fabric_event_deliveries (
    id              uuid PRIMARY KEY,
    group_name      text NOT NULL REFERENCES core.fabric_event_groups (name) ON DELETE CASCADE,
    sequence        bigint NOT NULL,
    attempt         int NOT NULL DEFAULT 1,
    available_at    timestamptz NOT NULL DEFAULT now(),
    leased_until    timestamptz NOT NULL
);

CREATE INDEX fabric_event_deliveries_waiting
    ON core.fabric_event_deliveries (group_name, available_at, leased_until);

-- Service Bus -------------------------------------------------------------------------------

CREATE TABLE core.fabric_queue_specs (
    address         text PRIMARY KEY,
    max_attempts    int NOT NULL,
    lease_s         bigint NOT NULL,
    max_depth       bigint NOT NULL
);

CREATE TABLE core.fabric_queue (
    id              uuid PRIMARY KEY,
    address         text NOT NULL,
    message         jsonb NOT NULL,
    attempts        int NOT NULL DEFAULT 0,
    available_at    timestamptz NOT NULL DEFAULT now(),
    leased_until    timestamptz,
    lease           uuid,
    queued_at       timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX fabric_queue_waiting ON core.fabric_queue (address, available_at, queued_at);
CREATE UNIQUE INDEX fabric_queue_lease ON core.fabric_queue (lease) WHERE lease IS NOT NULL;

CREATE TABLE core.fabric_dead_letters (
    id              uuid PRIMARY KEY,
    address         text NOT NULL,
    letter          jsonb NOT NULL,
    at              timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX fabric_dead_letters_address ON core.fabric_dead_letters (address, at);

-- Which process answers requests at an address. Request and reply is direct, in the process that
-- serves it; this is here so a request to an address served somewhere else says so plainly
-- instead of reading as nothing serving it at all.
CREATE TABLE core.fabric_services (
    address     text PRIMARY KEY,
    served_by   text NOT NULL,
    since       timestamptz NOT NULL DEFAULT now()
);

-- Cache Bus ---------------------------------------------------------------------------------

CREATE TABLE core.fabric_cache_specs (
    namespace       text PRIMARY KEY,
    default_ttl_s   bigint,
    max_entries     bigint NOT NULL
);

CREATE TABLE core.fabric_cache (
    namespace   text NOT NULL,
    key         text NOT NULL,
    value       jsonb NOT NULL,
    version     bigint NOT NULL,
    expires_at  timestamptz,
    written_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (namespace, key)
);

CREATE INDEX fabric_cache_expiry ON core.fabric_cache (namespace, expires_at);
CREATE INDEX fabric_cache_written ON core.fabric_cache (namespace, written_at);
