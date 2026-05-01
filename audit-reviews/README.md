# Audit Reviews

This directory holds the AI-Auditor cycle artefacts per `docs/development-operating-model.md` §2.

## File naming

```
YYYY-MM-DD-input.md      — change-set submitted by dev side to AI-Auditor
YYYY-MM-DD-verdict.md    — AI-Auditor reply (resolution per finding ID + sync gate decision)
YYYY-MM-DD-followup.md   — optional: dev response if verdict raised new questions
```

`YYYY-MM-DD` matches the date the input was submitted, not the date the verdict came back. Verdict and follow-up share the same date as the input they correspond to.

## Append-only

Once a verdict file is committed, it is never edited. Subsequent revisions go into a new dated file. This preserves the historical record of "what was asked and answered when" — auditors review change diffs against the prior decision baseline.

## Index

(Will be populated as audits accumulate.)

| Date | Input | Verdict | Mode-S sync gate | Notes |
|---|---|---|---|---|
| — | — | — | — | (no audits yet) |
