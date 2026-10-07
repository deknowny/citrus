<!-- Paste into AGENTS.md / CLAUDE.md -->
## Checks

Run checks through `citrus` (see skills/citrus/SKILL.md):
`citrus status` before checking, `citrus run` to check (proven results are
reused), `citrus wait last` after a lost session instead of re-running,
`citrus log last` for the first error instead of reading raw logs,
a `check` in the Citrus configuration for a new check instead of a wrapper script,
`citrus integrate --push` to bring in the base branch and publish,
`citrus tasks` / `citrus note` instead of asking other agents about their work.
