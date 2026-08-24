from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

if TYPE_CHECKING:
    from .virtual_input import VirtualMic

ProducerState = Literal["absent", "active", "stale", "underrunning"]


@dataclass(frozen=True, slots=True)
class AppVirtualInputSpec:
    id: str
    name: str
    uid: str
    sample_rate: Literal[48000]
    channels: Literal[1, 2]


@dataclass(frozen=True, slots=True)
class VirtualInputProducerStatus:
    app_id: str
    id: str
    uid: str
    kind: Literal["external_app"]
    state: ProducerState
    write_idx: int
    underrun_count: int
    attach_count: int
    generation: int


@dataclass(frozen=True, slots=True)
class AppVirtualInputs:
    app_id: str
    inputs: tuple[AppVirtualInputSpec, ...]


@dataclass(frozen=True, slots=True)
class PlanChange:
    kind: str
    target: str
    details: str


@dataclass(frozen=True, slots=True)
class ApplyPlan:
    changes: tuple[PlanChange, ...]
    warnings: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class ApplyResult:
    applied: bool
    plan: ApplyPlan
    warnings: tuple[str, ...]
    errors: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class VirtualMicInfo:
    uid: str
    ring_name: str
    sample_rate: Literal[48000]
    channels: Literal[1, 2]
    capacity_frames: int
    producer: VirtualInputProducerStatus


@dataclass(frozen=True, slots=True)
class SetVirtualInputsOutcome:
    apply: ApplyResult
    virtual_mics: tuple[VirtualMic, ...]
