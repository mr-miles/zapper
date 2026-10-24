---
name: analysis
description: "How an analyst works and publishes: a reproducible pipeline, units, checks, an assumptions table with ranges, calculations shown, sensitivity, and a dataset and report in their formats. Use it whenever you compute, model or publish numbers."
---

# Analysis

## Working

- A reproducible pipeline in `/work/analysis/`: one script that reads inputs and writes outputs. Raw inputs unchanged in `data/raw/`.
- Units on every column and figure; state the period and currency.
- Check: totals add up, magnitudes are plausible, a second method agrees where one exists.

## Assumptions and sensitivity

- Every assumption in a table: name, value, unit, source or rationale, confidence. Ranges (low, base, high) when uncertain. Data, assumptions and results never mixed.
- Show each calculation in words, then numbers: "cost = users × requests × price = 1,000 × 300 × $0.0002 = $60".
- Vary each important assumption across its range and show how the result moves, largest effect first; say what value would flip the decision.

## Publishing

- **Dataset**: CSV with a header row, one table per file, plain values, ISO dates, units in column names (`cost_usd`). A `dataset` with a summary of the columns and source (a README if needed), and the script.
- **Report**: a `report` with the assignment's thread:

```
# <Question>: analysis

Answer: the number or comparison, with range and confidence.

## Recommendation (if a decision was asked)
## Method
## Assumptions
| Assumption | Value | Unit | Source | Confidence |
## Results
## Sensitivity
## Limits
## Data and sources
```

Charts: axes with units, a title that states the finding, one message each, as images in the report's artifact. Your reply gives the answer, the range and the assumption that matters most.
