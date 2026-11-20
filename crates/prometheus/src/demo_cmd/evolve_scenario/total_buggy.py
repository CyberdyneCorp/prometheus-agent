"""Totals a ledger: one signed amount per line."""

import sys


def total(path):
    with open(path, encoding="utf-8") as ledger:
        amounts = [float(line) for line in ledger if line.strip()]
    return sum(amount for amount in amounts if amount > 0)


print(f"Total: {total(sys.argv[1]):.2f}")
