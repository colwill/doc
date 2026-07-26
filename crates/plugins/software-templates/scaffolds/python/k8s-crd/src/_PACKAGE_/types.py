"""The custom resource, as Python sees it. The CRD in `config/crd/bases` is the schema the cluster
enforces; these models are the same thing for the code that reads and writes the resources, and
`python -m {{ scaffold.package }} schema` prints the JSON schema they come to, so the two can be
compared rather than trusted."""

from __future__ import annotations

from enum import StrEnum
from typing import Annotated, Any

from pydantic import BaseModel, ConfigDict, Field

GROUP = "{{ scaffold.group }}"
VERSION = "v1alpha1"
PLURAL = "{{ scaffold.plural }}"
KIND = "{{ scaffold.kind }}"


class Phase(StrEnum):
    """Where a {{ scaffold.kind }} has got to."""

    PENDING = "Pending"
    READY = "Ready"
    FAILED = "Failed"


class Spec(BaseModel):
    """What somebody asks for."""

    model_config = ConfigDict(extra="forbid")

    owner: Annotated[str, Field(pattern=r"^[a-z][a-z0-9-]{1,62}$")]
    """The team answering for it, as the Catalogue in DOC names them."""

    size: Annotated[int, Field(ge=0, le=100)] = 1
    """How many of them to run."""

    retention: Annotated[str, Field(pattern=r"^[0-9]+(h|m|s)$")] = "168h"
    """How long what it makes is kept."""

    settings: dict[str, str] = Field(default_factory=dict)
    """Passed through to whatever runs it."""


class Status(BaseModel):
    """What is actually true of it."""

    model_config = ConfigDict(extra="allow")

    phase: Phase = Phase.PENDING
    reason: str | None = None
    observedGeneration: int = 0  # noqa: N815 - Kubernetes writes it in camelCase


class {{ scaffold.kind }}(BaseModel):
    """A whole resource, as the API server returns it."""

    model_config = ConfigDict(extra="allow")

    apiVersion: str = f"{GROUP}/{VERSION}"  # noqa: N815
    kind: str = KIND
    metadata: dict[str, Any] = Field(default_factory=dict)
    spec: Spec
    status: Status = Field(default_factory=Status)

    @property
    def name(self) -> str:
        return str(self.metadata.get("name", ""))
