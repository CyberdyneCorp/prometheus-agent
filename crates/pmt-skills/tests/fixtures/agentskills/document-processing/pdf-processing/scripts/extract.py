import sys

import pdfplumber

with pdfplumber.open(sys.argv[1]) as pdf:
    for page in pdf.pages:
        print(page.extract_text())
