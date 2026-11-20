"""Totals a ledger: one signed amount per line, debits included."""

import sys


def total(path):
    with open(path, encoding="utf-8") as ledger:
        amounts = [float(line) for line in ledger if line.strip()]
    return sum(amounts)


print(f"Total: {total(sys.argv[1]):.2f}")
