"""Shared authority policy and data types."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path


ORACLE_CRATES = {"greenlit-expr", "greenlit-workflow"}
TEST_BOUNDARY_CFG = "litci_test_boundaries"
OFFICIAL_RELEASE_COMMANDS = (
    Path(".github/workflows/release.yml"),
    Path("tools/release-check"),
)
SOURCE_TEST_PATTERN = re.compile(
    r"#\s*\[\s*(?:cfg(?:_attr)?\s*\([^]]*\btest\b[^]]*\)|"
    r"test\b|tokio\s*::\s*test\b)"
    r"|\bmod\s+tests\s*(?:;|\{)",
    re.MULTILINE,
)
IGNORED_PATTERN = re.compile(
    r"#\s*\[\s*(?:ignore\b|cfg_attr\s*\([^]]*\bignore\b)",
    re.MULTILINE,
)
TEST_ATTRIBUTE_PATTERN = re.compile(
    r"#\s*\[\s*(?:test|tokio\s*::\s*test)(?:\s*\([^]]*\))?\s*\]",
    re.MULTILINE,
)
INLINE_FEATURE_CFG_PATTERN = re.compile(
    r"#\s*\[\s*cfg(?:_attr)?\s*\([^]]*\bfeature\b[^]]*\)",
    re.MULTILINE,
)
OWN_TRAIT_PATTERN = re.compile(
    r"\bimpl(?:\s*<[^>{}]*>)?\s+"
    r"(?:[A-Za-z_][A-Za-z0-9_]*::)*"
    r"(?:ContainerEngine|RefResolver|ActionFetcher|RunnerProvider|Snapshotter|"
    r"RuntimeBundleFetcher|NodeBundleSpecs)\s+for\b",
    re.MULTILINE,
)
OWN_TYPE_PATTERN = re.compile(
    r"\b(?:struct|enum|type)\s+"
    r"(?:Fake|Mock|Stub|Scripted|Recording|Test|InMemory|Memory|Noop|Null)"
    r"[A-Za-z0-9_]*(?:Engine|Runtime|Resolver|Fetcher|Executor|Store|Planner|"
    r"Action|Bundle|Provider)[A-Za-z0-9_]*\b",
    re.MULTILINE,
)
EARLY_SUCCESS_PATTERN = re.compile(
    r"\breturn\s*(?:;|Ok\s*\(\s*\(\s*\)\s*\)\s*;)"
    r"|\b(?:std\s*::\s*)?process\s*::\s*exit\s*\(\s*0\s*\)",
    re.MULTILINE,
)
EXPLICIT_RETURN_PATTERN = re.compile(r"\breturn\b", re.MULTILINE)
PROCESS_SUCCESS_PATTERN = re.compile(
    r"\b(?:std\s*::\s*)?process\s*::\s*exit\s*\(\s*0\s*\)",
    re.MULTILINE,
)
STANDARD_MOD_PATTERN = re.compile(
    r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
    re.MULTILINE,
)
PATH_MOD_PATTERN = re.compile(
    r"#\s*\[\s*path\s*=\s*(?P<literal>r(?P<hashes>#+)?\".*?\"(?P=hashes)|"
    r"\"(?:\\.|[^\"\\])*\")\s*\]\s*"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*;",
    re.MULTILINE | re.DOTALL,
)


@dataclass(frozen=True)
class Violation:
    """One deterministic, source-located authority violation."""

    path: Path
    offset: int
    category: str
    detail: str


@dataclass(frozen=True)
class TargetSource:
    """One workspace Cargo target and the source path Cargo selected."""

    package: str
    package_root: Path
    name: str
    kinds: tuple[str, ...]
    source: Path
    required_features: tuple[str, ...]

    @property
    def is_test_code(self) -> bool:
        """Return whether Cargo treats this as a test or benchmark target."""

        return "test" in self.kinds or "bench" in self.kinds

    @property
    def is_capability_test(self) -> bool:
        """Return whether a feature gate owns this integration target."""

        return "test" in self.kinds and bool(self.required_features)


class GateError(Exception):
    """A concise command failure unrelated to an authority finding."""
