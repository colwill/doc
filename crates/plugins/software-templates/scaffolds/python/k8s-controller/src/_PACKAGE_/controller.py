"""Reconciling {{ scaffold.kind }}s: look at what was asked for, look at what the cluster has, and
make the second match the first. Every reconcile is a span, so a slow one shows up in the platform's
telemetry beside everything else."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any

import kopf
from kubernetes import client
from opentelemetry import trace

from .runtime import Config, Flags

logger = logging.getLogger(__name__)
tracer = trace.get_tracer("{{ values.name }}")

GROUP = "{{ scaffold.group }}"
VERSION = "v1alpha1"
PLURAL = "{{ scaffold.plural }}"


@dataclass
class Platform:
    """What `__main__` hands the handlers once telemetry and flags are up."""

    config: Config | None = None
    flags: Flags | None = None


PLATFORM = Platform()


@kopf.on.create(GROUP, VERSION, PLURAL)
@kopf.on.update(GROUP, VERSION, PLURAL)
@kopf.on.resume(GROUP, VERSION, PLURAL)
def reconcile(spec: kopf.Spec, name: str, namespace: str, patch: kopf.Patch, **_: Any) -> None:
    """Called for every change to a {{ scaffold.kind }}, and again when the controller restarts."""
    flags = PLATFORM.flags
    with tracer.start_as_current_span("reconcile") as span:
        span.set_attribute("resource.name", name)
        span.set_attribute("resource.namespace", namespace)

        # Turned off in DOC, this controller watches without changing anything: a way to stop it
        # acting without stopping it running.
        if flags is not None and not flags.boolean("reconcile", True):
            logger.info("reconciling is turned off in DOC; nothing was changed")
            patch.status["phase"] = "Pending"
            patch.status["reason"] = "Reconciling is turned off in DOC"
            return

        message = spec.get("message") or (
            flags.string("default-message", "Made by {{ values.name }}") if flags else "Made by {{ values.name }}"
        )
        wanted = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(name=name, namespace=namespace),
            data={"message": message, "size": str(spec.get("size", 1))},
        )
        # The owner reference means the config map goes when the resource does.
        kopf.adopt(wanted.to_dict())

        maps = client.CoreV1Api()
        try:
            maps.replace_namespaced_config_map(name, namespace, wanted)
        except client.ApiException as problem:
            if problem.status != 404:
                raise kopf.TemporaryError(f"the config map could not be written: {problem}", delay=30)
            maps.create_namespaced_config_map(namespace, wanted)

        patch.status["phase"] = "Ready"
        patch.status["reason"] = "Everything asked for is in place"
        logger.info("%s/%s is in place", namespace, name)


@kopf.on.delete(GROUP, VERSION, PLURAL)
def forget(name: str, namespace: str, **_: Any) -> None:
    """Kubernetes removes what this controller owns; anything outside the cluster goes here."""
    logger.info("%s/%s is gone", namespace, name)
