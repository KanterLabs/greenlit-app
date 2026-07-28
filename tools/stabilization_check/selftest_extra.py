"""Additional malformed parity-ledger command canaries."""

from __future__ import annotations


CaseShape = tuple[str, str, tuple[str, ...]]


def _row(cells: tuple[str, ...]) -> str:
    return f"| {' | '.join(cells)} |"


def _parity(header: tuple[str, ...], rows: tuple[tuple[str, ...], ...]) -> str:
    delimiter = tuple("---" for _ in header)
    return (
        "# Greenlit parity-exception ledger\n\n"
        f"{_row(header)}\n"
        f"{_row(delimiter)}\n"
        f"{chr(10).join(_row(value) for value in rows)}\n"
    )


def extra_parity_cases(
    header: tuple[str, ...],
    valid: tuple[str, ...],
) -> tuple[CaseShape, ...]:
    """Return strict-row, authority, anchor, and aggregate rejection inputs."""

    baseline = _parity(header, (valid,))
    missing_pipe = baseline.rstrip("\n")[:-1] + "\n"

    backslash = list(valid)
    backslash[6] = "Sh\\ane 2000-01-01"

    foreign_repository = list(valid)
    foreign_repository[4] = (
        "https://github.com/Other/repo/actions/runs/7; "
        f"source-commit={valid[2]}"
    )

    docs_authority = list(valid)
    docs_authority[4] = (
        "https://docs.github.com/definitely/not-real#fabricated; "
        f"source-commit={valid[2]}"
    )
    retained_authority = list(valid)
    retained_authority[4] = (
        "retained-run:fixtures/stabilization/parity/captures/"
        f"{valid[1]}-oracle.json; source-commit={valid[2]}"
    )
    bracket_identifier = list(valid)
    bracket_identifier[3] = '$.outputs[0].value["id"]'
    raw_control = list(valid)
    raw_control[3] = '$.outputs[0].value["bad\tkey"]'
    format_path = list(valid)
    format_path[3] = '$.outputs[0].value["safe\u202e"]'
    markup_path = list(valid)
    markup_path[3] = '$.outputs[0].value["<!--hidden-->"]'
    format_reason = list(valid)
    format_reason[5] += "\u202e"
    wrong_reason = list(valid)
    wrong_reason[5] = (
        "specification-permitted degradation "
        "(greenlit-v0-spec.md#content-and-environment-preparation): "
        "Greenlit emits wrong required semantic results and needs implementation repair"
    )
    entity_reason = list(valid)
    entity_reason[5] = (
        "specification-permitted degradation "
        "(greenlit-v0-spec.md#content-and-environment-preparation): "
        "Greenlit has a b&#117;g in required behavior and needs implementation repair"
    )

    nonexistent_anchor = list(valid)
    nonexistent_anchor[5] = nonexistent_anchor[5].replace(
        "#content-and-environment-preparation",
        "#not-a-real-spec-anchor",
    )

    conclusion = list(valid)
    conclusion[3] = "$.jobs[0].conclusion"

    category = list(valid)
    category[3] = "$.resource_security_findings[0].category"
    detail = list(valid)
    detail[0] = "GL-PARITY-002"
    detail[3] = "$.resource_security_findings[0].detail"
    table = baseline.splitlines()[2:]
    hidden = (
        "# Greenlit parity-exception ledger\n\n<!--\n"
        + "\n".join(table)
        + "\n# hidden table terminator\n-->\n"
    )
    split_table = baseline.replace(
        "\n| GL-PARITY-001",
        "\n\n| GL-PARITY-001",
        1,
    )

    return (
        (
            "missing trailing table pipe",
            missing_pipe,
            ("malformed parity exception table row",),
        ),
        (
            "backslash preservation",
            _parity(header, (tuple(backslash),)),
            ("Owner approval must be 'Shane YYYY-MM-DD'",),
        ),
        (
            "foreign Actions repository",
            _parity(header, (tuple(foreign_repository),)),
            ("Actions run authority names another repository",),
        ),
        (
            "documentation is not observation authority",
            _parity(header, (tuple(docs_authority),)),
            ("must be the exact validated GitHub Actions run URL",),
        ),
        (
            "retained replay is not live authority",
            _parity(header, (tuple(retained_authority),)),
            ("must be the exact validated GitHub Actions run URL",),
        ),
        (
            "noncanonical bracket identifier",
            _parity(header, (tuple(bracket_identifier),)),
            ("canonical comparator JSONPath spelling",),
        ),
        (
            "raw control in exception member",
            _parity(header, (tuple(raw_control),)),
            ("must be one canonical leaf JSONPath",),
        ),
        (
            "format control in exception member",
            _parity(header, (tuple(format_path),)),
            ("JSONPath members must be safe Markdown plain text",),
        ),
        (
            "markup in exception member",
            _parity(header, (tuple(markup_path),)),
            ("JSONPath members must be safe Markdown plain text",),
        ),
        (
            "format control in exception rationale",
            _parity(header, (tuple(format_reason),)),
            ("Reason and scope must be a substantive",),
        ),
        (
            "wrong-result rationale is not a degradation",
            _parity(header, (tuple(wrong_reason),)),
            ("never an in-scope defect or regression",),
        ),
        (
            "rendered entity cannot hide defect language",
            _parity(header, (tuple(entity_reason),)),
            ("Reason and scope must be a substantive",),
        ),
        (
            "comment-hidden exception table",
            hidden,
            ("must be a visible top-level Markdown table",),
        ),
        (
            "blank line terminates exception table",
            split_table,
            ("one contiguous visible Markdown table",),
        ),
        (
            "nonexistent spec anchor",
            _parity(header, (tuple(nonexistent_anchor),)),
            ("exact permitted v0 spec anchor",),
        ),
        (
            "failure-truth conclusion leaf",
            _parity(header, (tuple(conclusion),)),
            ("not a record, collection, identity, reference, or unknown field",),
        ),
        (
            "aggregate record laundering",
            _parity(header, (tuple(category), tuple(detail))),
            ("target multiple leaves in one semantic record",),
        ),
    )


__all__ = ["CaseShape", "extra_parity_cases"]
