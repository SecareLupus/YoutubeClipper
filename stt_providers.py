#!/usr/bin/env python3
"""Speech-to-text provider plugin interface and default stub implementation."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Type


@dataclass
class TranscribedSegment:
    text: str
    start: float
    end: float


class STTProviderError(RuntimeError):
    """Raised when a speech-to-text provider cannot fulfill a request."""


class STTProvider:
    """Base class for speech-to-text providers."""

    name: str = "stub"
    is_placeholder: bool = False

    def transcribe(self, audio_path: Path, language: str) -> List[TranscribedSegment]:
        raise NotImplementedError


_PROVIDER_REGISTRY: Dict[str, Type[STTProvider]] = {}


def register_provider(cls: Type[STTProvider]) -> Type[STTProvider]:
    """Class decorator to register STT providers by name."""

    if not cls.name:
        raise ValueError("STT providers must define a non-empty 'name' attribute.")
    _PROVIDER_REGISTRY[cls.name] = cls
    return cls


def available_providers() -> List[str]:
    """Return the list of registered provider names."""

    return sorted(_PROVIDER_REGISTRY)


def get_stt_provider(name: str) -> STTProvider:
    """Instantiate a provider by name."""

    try:
        provider_cls = _PROVIDER_REGISTRY[name]
    except KeyError as exc:  # pragma: no cover - defensive guard
        options = ", ".join(available_providers()) or "none"
        raise STTProviderError(
            f"Unknown STT provider '{name}'. Available providers: {options}."
        ) from exc
    return provider_cls()


@register_provider
class StubSTTProvider(STTProvider):
    """Placeholder provider that documents how to plug in a real implementation."""

    name = "stub"
    is_placeholder = True

    def transcribe(self, audio_path: Path, language: str) -> List[TranscribedSegment]:
        # TODO: Replace this stub with an optional implementation that calls a real STT API.
        raise STTProviderError(
            "The 'stub' speech-to-text provider is not implemented. "
            "Install or implement a real provider and select it via '--stt-provider'."
        )
