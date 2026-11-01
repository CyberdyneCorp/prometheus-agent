---
name: pdf-processing
description: Extract text and tables from PDF files, fill PDF forms and merge documents.
license: Apache-2.0
compatibility: Requires python3 with pdfplumber installed.
allowed-tools: Bash(python3:*) Read
metadata:
  author: example-org
  version: "1.0"
---

# PDF processing

Use `scripts/extract.py` to pull text out of a PDF:

```bash
python3 ${PROMETHEUS_SKILL_DIR}/scripts/extract.py input.pdf
```

See [the reference](references/REFERENCE.md) for form filling.
