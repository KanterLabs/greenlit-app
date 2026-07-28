"""Rendered-Markdown topology canaries for governance ledgers."""

from __future__ import annotations


CaseShape = tuple[str, str, str, tuple[str, ...]]
ValidShape = tuple[str, str, str]


def _wrapped(
    document: str,
    opener: str,
    closer: str,
    *,
    blank_before_close: bool = False,
) -> str:
    title, separator, remainder = document.partition("\n\n")
    if not separator:
        raise ValueError("self-test document has no title separator")
    blank = "\n" if blank_before_close else ""
    return f"{title}\n\n{opener}\n{remainder}{blank}{closer}\n"


def _fenced(
    document: str,
    opener: str,
    false_close: str,
    closer: str,
) -> str:
    title, separator, remainder = document.partition("\n\n")
    if not separator:
        raise ValueError("self-test document has no title separator")
    return f"{title}\n\n{opener}\n{false_close}\n{remainder}{closer}\n"


def _before_table(document: str, prefix: str) -> str:
    title, separator, remainder = document.partition("\n\n")
    if not separator:
        raise ValueError("self-test document has no title separator")
    return f"{title}\n{prefix}\n\n{remainder}"


def markdown_valid_cases(
    stabilization: str,
    parity: str,
) -> tuple[ValidShape, ...]:
    """Pin dollar-delimiter forms that GitHub renders with a real table."""

    # GitHub's POST /markdown GFM renderer emits a table for these exact
    # samples; standalone dollar lines do not form a persistent container.
    stabilization_dollars = _before_table(stabilization, "$$") + "\n$$\n"
    parity_dollars = (
        _before_table(parity, "$$")
        + "\n## Ledger authority\n\n$$\n"
    )
    lookalikes = "\\$$\n$$value$$\n    $$"
    return (
        (
            "dollar lines around stabilization table",
            stabilization_dollars,
            parity,
        ),
        (
            "dollar lines around parity table",
            stabilization,
            parity_dollars,
        ),
        (
            "nonpersistent dollar delimiter edges",
            _before_table(stabilization, lookalikes),
            _before_table(parity, lookalikes),
        ),
    )


def markdown_cases(
    stabilization: str,
    parity: str,
) -> tuple[CaseShape, ...]:
    """Return public canaries for rendered-table concealment."""

    malformed = stabilization.replace(
        "|---|---|---:|", "|---|---|--|", 1
    )
    raw_blocks = (
        ("type-1 script", "<script>", "</script>", False),
        ("type-2 comment", "<!--", "-->", False),
        ("type-3 instruction", "<?greenlit", "?>", False),
        ("type-4 declaration", "<!GREENLIT", ">", False),
        ("type-5 CDATA", "<![CDATA[", "]]>", False),
        ("type-6 table", "<table>", "</table>", True),
        ("type-7 custom tag", "<x-widget>", "</x-widget>", True),
    )
    cases: list[CaseShape] = [
        (
            "malformed Markdown separator",
            malformed,
            parity,
            ("malformed Markdown table separator",),
        ),
        (
            "non-CommonMark stabilization line separator",
            stabilization.replace("\n", "\u2028"),
            parity,
            ("non-CommonMark line/control character U+2028",),
        ),
        (
            "non-CommonMark parity line separator",
            stabilization,
            parity.replace("\n", "\u2028"),
            ("non-CommonMark line/control character U+2028",),
        ),
        (
            "lazy list stabilization table",
            stabilization.replace(
                "\n| Defect ID", "\n- conceal\n| Defect ID", 1
            ),
            parity,
            ("table header must be preceded by a blank CommonMark line",),
        ),
        (
            "lazy blockquote stabilization table",
            stabilization.replace(
                "\n| Defect ID", "\n> conceal\n| Defect ID", 1
            ),
            parity,
            ("table header must be preceded by a blank CommonMark line",),
        ),
        (
            "lazy list parity table",
            stabilization,
            parity.replace(
                "\n| Exception ID", "\n- conceal\n| Exception ID", 1
            ),
            (
                "parity exception table header must be preceded by a blank "
                "CommonMark line",
            ),
        ),
        (
            "long backtick fence",
            _fenced(stabilization, "````python", "```", "````"),
            parity,
            ("table must be visible top-level Markdown",),
        ),
        (
            "long tilde fence",
            _fenced(stabilization, "~~~~python", "~~~", "~~~~"),
            parity,
            ("table must be visible top-level Markdown",),
        ),
        (
            "indented false fence close",
            _fenced(stabilization, "```", "    ```", "```"),
            parity,
            ("table must be visible top-level Markdown",),
        ),
        (
            "annotated false fence close",
            _fenced(stabilization, "```", "```still-open", "```"),
            parity,
            ("table must be visible top-level Markdown",),
        ),
    ]
    cases.extend(
        (
            f"raw HTML {name} stabilization table",
            _wrapped(
                stabilization,
                opener,
                closer,
                blank_before_close=blank,
            ),
            parity,
            ("table must be visible top-level Markdown",),
        )
        for name, opener, closer, blank in raw_blocks
    )
    return tuple(cases)


__all__ = [
    "CaseShape",
    "ValidShape",
    "markdown_cases",
    "markdown_valid_cases",
]
