-- TaskWorker's two durable pools: cron schedules and the background queue. Immediate tasks run
-- inside the backend and frontend within a deadline and are never written down.

CREATE TABLE core.cron_tasks (
    name          text PRIMARY KEY,
    schedule      text NOT NULL,
    description   text,
    paused        boolean NOT NULL DEFAULT false,
    next_run_at   timestamptz,
    last_run_at   timestamptz,
    last_state    text,
    last_error    text,
    -- One replica holds a run at a time; the lease is what stops a second one starting the same run.
    claimed_by    text,
    claimed_until timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX cron_tasks_due_idx ON core.cron_tasks (next_run_at) WHERE paused = false;

CREATE TABLE core.tasks (
    id               uuid PRIMARY KEY,
    kind             text NOT NULL,
    state            text NOT NULL
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    payload          jsonb NOT NULL DEFAULT '{}'::jsonb,
    result           jsonb,
    error            text,
    attempts         integer NOT NULL DEFAULT 0,
    max_attempts     integer NOT NULL DEFAULT 3,
    -- Cancelling asks the task to stop; the deadline is what turns the ask into a failure.
    cancel_requested boolean NOT NULL DEFAULT false,
    started_by_kind  text NOT NULL,
    started_by_id    text,
    started_by_label text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    started_at       timestamptz,
    finished_at      timestamptz,
    -- Held while a worker is running it. A crash leaves this in the past and the task is requeued.
    lease_until      timestamptz,
    updated_at       timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX tasks_state_idx ON core.tasks (state, created_at DESC);
CREATE INDEX tasks_kind_idx ON core.tasks (kind, created_at DESC);
CREATE INDEX tasks_owner_idx ON core.tasks (started_by_kind, started_by_id, created_at DESC);
CREATE INDEX tasks_stranded_idx ON core.tasks (lease_until) WHERE state = 'running';

-- Pausing a whole kind of background task, which the API offers alongside pausing one schedule.
CREATE TABLE core.task_kinds (
    kind       text PRIMARY KEY,
    paused     boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- What plugins may read: their own tasks carry no credentials, but the caller is named, so the
-- view keeps the label and drops the identifier.
CREATE VIEW core_v1.tasks AS
SELECT id, kind, state, payload, result, error, attempts, max_attempts, cancel_requested,
       started_by_kind, started_by_label, created_at, started_at, finished_at
FROM core.tasks;
