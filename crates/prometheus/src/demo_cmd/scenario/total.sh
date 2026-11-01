#!/bin/sh
# Prints the balance of ledger.txt: credits add to it, debits subtract
# from it. Amounts are summed in cents, so no floating point is involved.
total=0
while read -r kind amount _; do
    cents=$((${amount%.*} * 100 + 1${amount#*.} - 100))
    case $kind in
    credit) total=$((total + cents)) ;;
    debit) total=$((total + cents)) ;;
    esac
done <ledger.txt
printf 'Total: %d.%02d\n' $((total / 100)) $((total % 100))
