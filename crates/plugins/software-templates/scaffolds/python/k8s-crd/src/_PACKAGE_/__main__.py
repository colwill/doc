"""{{ values.name }} — {{ values.description }}

The custom resource lives in `types.py`; this reads and writes them, so the models are used by
something from the first commit.

```sh
python -m {{ scaffold.package }} schema    # the JSON schema the models come to
python -m {{ scaffold.package }} list      # what is in the cluster now
python -m {{ scaffold.package }} create my-thing
```
"""

from __future__ import annotations

import json
import os
import sys

from kubernetes import client, config as kubeconfig

from .runtime import Config, Flags, start_telemetry
from .types import GROUP, PLURAL, VERSION, {{ scaffold.kind }}, Spec


def main() -> int:
    command = sys.argv[1] if len(sys.argv) > 1 else "list"
    if command == "schema":
        print(json.dumps({{ scaffold.kind }}.model_json_schema(), indent=2))
        return 0

    settings = Config.load()
    shutdown = start_telemetry(settings)
    flags = Flags.start(settings)
    namespace = os.environ.get("NAMESPACE", "default")
    try:
        kubeconfig.load_config()
        api = client.CustomObjectsApi()

        if command == "create":
            name = sys.argv[2] if len(sys.argv) > 2 else "{{ values.name }}-sample"
            owner = flags.string("default-owner", "{{ values.team | name | default('platform') }}")
            wanted = {{ scaffold.kind }}(metadata={"name": name}, spec=Spec(owner=owner, size=1))
            made = api.create_namespaced_custom_object(
                GROUP, VERSION, namespace, PLURAL, wanted.model_dump(exclude_none=True)
            )
            print(f"{{ scaffold.kind }}/{made['metadata']['name']} created in {namespace}")
            return 0

        held = api.list_namespaced_custom_object(GROUP, VERSION, namespace, PLURAL)
        items = [{{ scaffold.kind }}.model_validate(one) for one in held.get("items", [])]
        if not items:
            print(f"there are no {{ scaffold.plural }} in {namespace}")
        for one in items:
            print(f"{one.name:<30} owner={one.spec.owner:<20} size={one.spec.size} phase={one.status.phase}")
        return 0
    finally:
        flags.stop()
        shutdown()


if __name__ == "__main__":
    raise SystemExit(main())
