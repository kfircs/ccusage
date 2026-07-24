# Antigravity CLI (agy) Source

Data source:

```text
${AGY_DATA_DIR:-~/.gemini/antigravity-cli}/conversations/*.db
```

Antigravity CLI stores each conversation as a SQLite database. ccusage opens
the databases read-only and parses the `steps` table (protobuf `step_payload`)
plus the `gen_metadata` table. The `conversation_summaries.db` metadata file
and older `.pb` conversation files are ignored. Token mapping (protobuf field
paths inside the `f5` event envelope's `f9` usage submessage):

- `inputTokens` <- per-turn delta of cumulative input tokens (`f5.f9.f5`)
- `outputTokens` <- `f5.f9.f2`
- `extraTotalTokens` (thinking) <- `f5.f9.f3`, billed at the output rate

The model is resolved from the per-conversation `gen_metadata` enum → name
mapping. Checkpoint/compaction steps carry an unnamed internal enum and are
attributed to the conversation's primary model (the most common model-response
enum). Request timestamps come from the nested `f5.f1` submessage
(`{seconds, nanos}`).

Costs are calculated from LiteLLM pricing using the resolved model name. Set
`AGY_DATA_DIR` to override the data root, or a comma-separated list of roots.

## Aliases and pricing overrides

Antigravity emits variant model ids that LiteLLM does not list verbatim. ccusage
maps them to the known names it does price (the displayed model name stays the
real agy variant; only the pricing lookup is redirected):

| agy model id              | priced as                        |
| ------------------------- | -------------------------------- |
| `gpt-oss-120b-medium`     | `openrouter/openai/gpt-oss-120b` |
| `gemini-3-flash-a`        | `gemini-3-flash`                 |
| `gemini-default`          | `gemini-3-flash`                 |
| `gemini-3.1-pro-low`      | `gemini-3.1-pro`                 |
| `gemini-3.6-flash-tiered` | `gemini-3.6-flash`               |

`gpt-oss-120b-medium` prices today via the OpenRouter alias. The gemini
variants resolve to their base names but are only priced once LiteLLM carries
`gemini-3-flash` / `gemini-3.1-pro` / `gemini-3.6-flash`; until then they show
`$0.00` and a missing-pricing warning.

For any id that still lacks a price, supply per-model costs in your ccusage
config under the `agy` section's `pricingOverrides` map:

```json
{
  "agy": {
    "pricingOverrides": {
      "gemini-3-flash-a": {
        "inputCostPerToken": 0.00000030,
        "outputCostPerToken": 0.00000010
      }
    }
  }
}
```