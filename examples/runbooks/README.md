---
title: Runbooks
runbook: false
---
# Runbooks

What to do when something in DOC goes wrong, written so that **Agent Smith** can run each one: press
**Run with Agent Smith** on a runbook's page and choose the environment. It works out which
situation applies from what DOC can see, does the steps it has tools for, and tells you exactly
what is left to do by hand. Against **production** it only reads.

They follow the **Backend** architecture view: the frontend calls the backend; the backend calls
the data store, publishes through `doc-event-source` onto the event, service and cache buses, and
subscribes to the event bus.

| Runbook | For | When |
|---|---|---|
| [The DOC backend is unreachable](backend-unreachable.md) | Production | Every page says Service Unavailable |
| [The data store is degraded](data-store-degraded.md) | Production | Pages are slow or fail to save |
| [Events are not being delivered](events-not-delivered.md) | Production | Automations, jobs and live updates stop |
| [A plugin's pages say Service Unavailable](plugin-unavailable.md) | Production | One plugin is down, the rest are fine |
| [Turn on debug logging for the backend](backend-debug-logging.md) | Development | You need the backend's detail for a while |
| [Cloudy Cheer's job is failing](cloudy-cheer-failing.md) | Test | The experiment stops cheering anyone up |
| [CodeCaChe's code map is stale](codecache-stale.md) | Development | Search and insights miss recent code |
