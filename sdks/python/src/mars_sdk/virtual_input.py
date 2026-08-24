from __future__ import annotations

import json
from typing import Any, TYPE_CHECKING

from .models import VirtualMicInfo

if TYPE_CHECKING:
    from ._native import LiveWriter

class VirtualMic:
    __slots__ = ("info", "_wire_info")

    def __init__(self, info: VirtualMicInfo, wire_info: dict[str, Any]) -> None:
        self.info = info
        self._wire_info = wire_info

    def open_live_writer(self) -> LiveWriter:
        from ._native import LiveWriter

        return LiveWriter(json.dumps(self._wire_info, separators=(",", ":")))
