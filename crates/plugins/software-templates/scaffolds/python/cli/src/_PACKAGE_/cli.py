"""What {{ values.name }} does. Each command is one function, and `run` chooses between them, so
adding one is adding an entry to the table below."""

from __future__ import annotations

import argparse
from typing import Callable

from opentelemetry import trace

from .runtime import Config, Flags

tracer = trace.get_tracer("{{ values.name }}")


def hello(config: Config, flags: Flags, arguments: argparse.Namespace) -> int:
    with tracer.start_as_current_span("hello"):
        # A flag read here is DOC's, with the value this service falls back to beside it.
        greeting = flags.string("greeting", "Hello")
        if flags.boolean("shout", False):
            greeting = greeting.upper()
        print(f"{greeting}, {arguments.who} — from {config.service} in {config.environment}")
    return 0


def show_flags(config: Config, flags: Flags, arguments: argparse.Namespace) -> int:
    print(f"{config.service} reads its flags from {config.flags_url}")
    print(f"  greeting = {flags.string('greeting', 'Hello')!r}")
    print(f"  shout    = {flags.boolean('shout', False)}")
    if not config.flags_token:
        print("note: DOC_FLAGS_TOKEN is not set, so only public flags are read")
    return 0


Command = Callable[[Config, Flags, argparse.Namespace], int]


def run(config: Config, flags: Flags, argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="{{ values.name }}", description="{{ values.description }}")
    commands = parser.add_subparsers(dest="command", required=True)

    greeting = commands.add_parser("hello", help="Says hello, and shows what the platform is telling this service")
    greeting.add_argument("--who", default="world", help="who to greet")
    greeting.set_defaults(run=hello)

    listing = commands.add_parser("flags", help="Prints every flag and setting DOC holds for this service")
    listing.set_defaults(run=show_flags)

    arguments = parser.parse_args(argv)
    command: Command = arguments.run
    return command(config, flags, arguments)
