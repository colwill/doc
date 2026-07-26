"""{{ values.name }} — {{ values.description }}"""

from __future__ import annotations

import sys

from .cli import run
from .runtime import Config, Flags, start_telemetry


def main() -> int:
    config = Config.load()
    # A command still runs when the collector cannot be reached; it is only not measured.
    try:
        shutdown = start_telemetry(config)
    except Exception as problem:  # noqa: BLE001
        print(f"telemetry is not being exported: {problem}", file=sys.stderr)
        shutdown = lambda: None  # noqa: E731

    flags = Flags.start(config)
    try:
        return run(config, flags, sys.argv[1:])
    finally:
        flags.stop()
        shutdown()


if __name__ == "__main__":
    raise SystemExit(main())
