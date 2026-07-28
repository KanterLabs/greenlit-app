"""Criterion manifest, measurement, and fixed-host budget authority."""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any


ENTRY_KEYS = {"package", "target", "name", "samples", "max_time_ns"}
TOP_LEVEL_KEYS = {"schema_version", "benchmarks"}

Number = int | float
Measurement = tuple[int, Number]
Observation = tuple[str, str, str, Number, Number]


class GateError(Exception):
    """A concise benchmark-gate failure."""


def ensure_finite_json(value: Any, location: str) -> None:
    if isinstance(value, float) and not math.isfinite(value):
        raise GateError(f"{location}: JSON number must be finite")
    if isinstance(value, list):
        for index, item in enumerate(value):
            ensure_finite_json(item, f"{location}[{index}]")
    elif isinstance(value, dict):
        for key, item in value.items():
            ensure_finite_json(item, f"{location}.{key}")


def parse_json(raw: str, source: str) -> Any:
    """Parse strict JSON with unique keys and finite numeric values."""

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise GateError(f"{source}: duplicate JSON key {key!r}")
            result[key] = value
        return result

    def reject_nonfinite_constant(value: str) -> None:
        raise GateError(f"{source}: invalid non-finite JSON number {value!r}")

    try:
        value = json.loads(
            raw,
            object_pairs_hook=unique_object,
            parse_constant=reject_nonfinite_constant,
        )
    except json.JSONDecodeError as error:
        raise GateError(f"{source}: invalid JSON: {error}") from error
    ensure_finite_json(value, source)
    return value


def positive_number(value: Any, location: str) -> Number:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or value <= 0
    ):
        raise GateError(f"{location} must be a positive finite number")
    return value


def load_manifest(manifest: Path) -> list[dict[str, Any]]:
    """Load the exact schema-v2 benchmark inventory and budgets."""

    try:
        raw = manifest.read_text(encoding="utf-8")
    except OSError as error:
        raise GateError(f"could not read {manifest}: {error}") from error
    value = parse_json(raw, str(manifest))

    if not isinstance(value, dict) or set(value) != TOP_LEVEL_KEYS:
        raise GateError(
            f"{manifest}: top level must contain exactly "
            f"{sorted(TOP_LEVEL_KEYS)!r}"
        )
    if value["schema_version"] != 2:
        raise GateError(f"{manifest}: schema_version must be 2")
    entries = value["benchmarks"]
    if not isinstance(entries, list) or not entries:
        raise GateError(f"{manifest}: benchmarks must be a nonempty array")

    identities: set[tuple[str, str, str]] = set()
    for index, entry in enumerate(entries):
        location = f"{manifest}: benchmarks[{index}]"
        if not isinstance(entry, dict) or set(entry) != ENTRY_KEYS:
            raise GateError(f"{location} must contain exactly {sorted(ENTRY_KEYS)!r}")
        for field in ("package", "target", "name"):
            text = entry[field]
            if not isinstance(text, str) or not text or text.strip() != text:
                raise GateError(f"{location}.{field} must be nonempty trimmed text")
        samples = entry["samples"]
        if isinstance(samples, bool) or not isinstance(samples, int) or samples < 10:
            raise GateError(
                f"{location}.samples must be an integer of at least 10 "
                "(Criterion's nonzero minimum)"
            )
        positive_number(entry["max_time_ns"], f"{location}.max_time_ns")
        identity = (entry["package"], entry["target"], entry["name"])
        if identity in identities:
            raise GateError(f"{location} duplicates benchmark identity {identity!r}")
        identities.add(identity)
    return entries


def listed_names(output: str) -> list[str]:
    """Extract exact benchmark names from Criterion's terse list output."""

    suffix = ": benchmark"
    names: list[str] = []
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.endswith(suffix):
            names.append(stripped[: -len(suffix)].strip())
    return names


def load_measurements(criterion_home: Path) -> dict[str, Measurement]:
    """Load exact sample counts and slope upper confidence bounds."""

    observed: dict[str, Measurement] = {}
    for metadata_path in criterion_home.rglob("new/benchmark.json"):
        sample_path = metadata_path.with_name("sample.json")
        estimates_path = metadata_path.with_name("estimates.json")
        try:
            metadata_raw = metadata_path.read_text(encoding="utf-8")
            sample_raw = sample_path.read_text(encoding="utf-8")
            estimates_raw = estimates_path.read_text(encoding="utf-8")
        except OSError as error:
            raise GateError(f"could not read Criterion output: {error}") from error
        metadata = parse_json(metadata_raw, str(metadata_path))
        sample = parse_json(sample_raw, str(sample_path))
        estimates = parse_json(estimates_raw, str(estimates_path))

        name = metadata.get("full_id") if isinstance(metadata, dict) else None
        iterations = sample.get("iters") if isinstance(sample, dict) else None
        times = sample.get("times") if isinstance(sample, dict) else None
        if not isinstance(name, str) or not name:
            raise GateError(f"{metadata_path}: missing nonempty full_id")
        if name in observed:
            raise GateError(f"Criterion emitted duplicate benchmark {name!r}")
        if not isinstance(iterations, list) or not isinstance(times, list):
            raise GateError(f"{sample_path}: missing sample arrays")
        if not iterations or len(iterations) != len(times):
            raise GateError(f"{sample_path}: sample count is zero or inconsistent")
        slope = estimates.get("slope") if isinstance(estimates, dict) else None
        confidence = (
            slope.get("confidence_interval") if isinstance(slope, dict) else None
        )
        if not isinstance(confidence, dict):
            raise GateError(f"{estimates_path}: missing slope confidence interval")
        confidence_level = positive_number(
            confidence.get("confidence_level"),
            f"{estimates_path}: slope confidence level",
        )
        if confidence_level >= 1:
            raise GateError(
                f"{estimates_path}: slope confidence level must be less than 1"
            )
        lower_bound = positive_number(
            confidence.get("lower_bound"),
            f"{estimates_path}: slope lower confidence bound",
        )
        upper_bound = positive_number(
            confidence.get("upper_bound"),
            f"{estimates_path}: slope upper confidence bound",
        )
        point_estimate = positive_number(
            slope.get("point_estimate") if isinstance(slope, dict) else None,
            f"{estimates_path}: slope point estimate",
        )
        if not lower_bound <= point_estimate <= upper_bound:
            raise GateError(
                f"{estimates_path}: slope estimate is outside its confidence interval"
            )
        observed[name] = (len(iterations), upper_bound)
    return observed


def enforce_budgets(
    package: str,
    target: str,
    entries: list[dict[str, Any]],
    sample_count: int,
    measurements: dict[str, Measurement],
) -> tuple[int, list[Observation]]:
    """Require the exact samples and conservative estimate for every budget."""

    expected_names = {entry["name"] for entry in entries}
    if set(measurements) != expected_names:
        raise GateError(
            f"{package}/{target}: sampled benchmarks differ from manifest; "
            f"missing={sorted(expected_names - set(measurements))!r}, "
            f"unexpected={sorted(set(measurements) - expected_names)!r}"
        )
    observations: list[Observation] = []
    for entry in entries:
        name = entry["name"]
        actual_count, upper_bound = measurements[name]
        if actual_count != sample_count:
            raise GateError(
                f"{package}/{target}/{name}: expected {sample_count} samples, "
                f"found {actual_count}"
            )
        budget = entry["max_time_ns"]
        if upper_bound > budget:
            raise GateError(
                f"{package}/{target}/{name}: Criterion slope upper confidence "
                f"bound {upper_bound:.2f} ns exceeds the fixed-host budget "
                f"{budget:.2f} ns; restore benchmark latency or update the "
                "manifest only with new pinned-host evidence"
            )
        observations.append((package, target, name, upper_bound, budget))
    return sum(count for count, _ in measurements.values()), observations
