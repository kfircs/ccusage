# Agy Data Source (Beta)

> Agy support is experimental while the Antigravity CLI database format continues to evolve.

ccusage can read Antigravity CLI conversation databases as one of its supported local data sources. Agy uses the same unified and focused report model as Claude Code, Codex, OpenCode, Amp, Droid, Codebuff, Hermes Agent, pi-agent, Goose, OpenClaw, Kilo, Kimi, Qwen, GitHub Copilot CLI, and Gemini CLI.

## Focused Views

::: code-group

```bash [bunx (Recommended)]
bunx ccusage agy --help
```

```bash [npx]
npx ccusage@latest agy --help
```

```bash [pnpm]
pnpm dlx ccusage agy --help
```

:::

## Data Source

The CLI reads Agy SQLite conversation databases from `AGY_DATA_DIR` (defaults to `~/.gemini/antigravity-cli`). `AGY_DATA_DIR` can be one directory or a comma-separated list of directories.

```bash
AGY_DATA_DIR="$HOME/.gemini/antigravity-cli,/backup/agy" ccusage agy daily
```

```text
~/.gemini/antigravity-cli/
└── conversations/
    └── *.db
```

ccusage ignores the metadata `conversation_summaries.db` file and any older `.pb` conversation files that predate the SQLite format.

## Report Views

| Focused view          | Description                        | See also                                |
| --------------------- | ---------------------------------- | --------------------------------------- |
| `ccusage agy daily`   | Aggregate usage by day             | [Daily Usage](/guide/daily-reports)     |
| `ccusage agy weekly`  | Aggregate usage by week            | [Weekly Usage](/guide/weekly-reports)   |
| `ccusage agy monthly` | Aggregate usage by month           | [Monthly Usage](/guide/monthly-reports) |
| `ccusage agy session` | Group usage by Agy conversation    | [Session Usage](/guide/session-reports) |

These views support `--json` for structured output, `--compact` for narrow terminals, and `--offline` for cached pricing data.

## What Gets Calculated

- **Token usage** - Agy `steps` rows expose model response and checkpoint steps with input, output, and thinking token counts.
- **Input tokens** - Agy records cumulative input tokens per conversation; ccusage turns those cumulative values into per-turn input deltas.
- **Reasoning tokens** - Agy thinking tokens are included in total tokens and priced as output tokens when pricing data is available.
- **Pricing** - Costs are calculated from LiteLLM pricing data using the raw model name resolved from Agy's per-conversation `gen_metadata` mapping.

## Privacy

Agy data is parsed entirely on your machine. ccusage opens the local SQLite databases in read-only mode and never uploads conversation content, prompts, or usage data.

## Environment Variables

| Variable        | Description                                                                            |
| --------------- | -------------------------------------------------------------------------------------- |
| `AGY_DATA_DIR`  | Override the root directory, or comma-separated root directories, containing Agy data |
| `LOG_LEVEL`     | Adjust verbosity (0 silent ... 5 trace)                                                |

## Troubleshooting

::: details No Agy usage data found
Ensure Agy has written SQLite conversation databases under `~/.gemini/antigravity-cli/conversations/`. Set `AGY_DATA_DIR` if your Agy data lives elsewhere or in multiple archive roots.
:::

::: details Costs showing as $0.00
If a model is not in LiteLLM's database, the cost will be $0.00. Use `--offline=false` to fetch the latest pricing data, or [open an issue](https://github.com/ccusage/ccusage/issues/new) to request alias support.
:::
