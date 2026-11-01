# Prints the balance of ledger.txt: credits add to it, debits subtract
# from it. Amounts are summed in cents, so no floating point is involved.
$total = 0
foreach ($line in Get-Content ledger.txt) {
    $kind, $amount, $note = -split $line
    $cents = [int]($amount -replace '\.', '')
    switch ($kind) {
        credit { $total += $cents }
        debit { $total += $cents }
    }
}
'Total: {0}.{1:D2}' -f [math]::Floor($total / 100), ($total % 100)
