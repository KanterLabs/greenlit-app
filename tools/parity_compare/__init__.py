"""Strict comparison support for canonical parity observations."""


class ContractError(Exception):
    """An input violates the canonical parity comparison contract."""


__all__ = ["ContractError"]
