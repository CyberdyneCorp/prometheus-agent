"""``agent.account_usage``: the snapshot types a profile's ``fetch_account_usage`` returns
(documented in the Hermes model-provider guide)."""

from dataclasses import dataclass
from datetime import datetime
from typing import Optional


@dataclass(frozen=True)
class AccountUsageWindow:
    label: str
    used_percent: Optional[float] = None
    reset_at: Optional[datetime] = None
    detail: Optional[str] = None
    scope: str = "account"


@dataclass(frozen=True)
class AccountUsageSnapshot:
    provider: str
    source: str
    fetched_at: datetime
    title: str = "Account limits"
    plan: Optional[str] = None
    windows: tuple = ()
    details: tuple = ()
    unavailable_reason: Optional[str] = None
    identity: Optional[str] = None
    raw: Optional[dict] = None
    usage_window: bool = False
