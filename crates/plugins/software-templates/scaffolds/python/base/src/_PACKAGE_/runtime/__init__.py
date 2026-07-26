"""What every service on this platform has: its settings, its telemetry and its feature flags.

DOC wrote this when the service was created; it is yours to change.
"""

from .config import Config
from .flags import Flags
from .telemetry import start_telemetry

__all__ = ["Config", "Flags", "start_telemetry"]
