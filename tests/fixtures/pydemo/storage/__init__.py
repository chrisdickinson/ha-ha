"""Storage boundary fixture."""

from dataclasses import dataclass
from typing import Protocol


@dataclass
class User:
    """A user record."""

    id: int
    name: str


class Repository(Protocol):
    """The storage boundary the domain layer consumes."""

    def find_user(self, id: int) -> User | None:
        """Find a single user by id."""
        ...

    def list_users(self) -> list[User]:
        """List every user."""
        ...


def _open_pool(url: str, max_conns: int) -> str:
    """Not part of the boundary — a private helper."""
    return f"{url}:{max_conns}"
