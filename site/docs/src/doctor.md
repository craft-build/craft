# Doctor

`craft doctor` checks that your provider and model are working. If they aren't, it tries alternatives and remembers the first one that works.

This fixes the common "Craft is broken" reports where the real cause is an expired key, a renamed model, or a downed provider.

## Run It

```bash
craft doctor
```

Craft pings the current model with a minimal call and reports the result:

```
craft 0.6.5 on macos aarch64
config dir: /home/user/.config/craft
state dir:  /home/user/.local/state/craft

✓ current model: anthropic/claude-sonnet-4-6 (ok)

providers:
  anthropic: ok (current)
  openai: unavailable — authentication failed, run `craft auth login` or check your API key
```

## Self-Healing

When the current model fails, Craft iterates other providers in a fallback order (Anthropic, OpenAI, Copilot, Synthetic, DeepSeek, then the rest). For each, it builds the provider, picks a strong-tier model, and pings it.

The first provider that responds is saved as your model. The next `craft` run picks it up automatically.

```
✗ current model: anthropic/claude-opus-4-6 (fail — context length exceeded)
✓ self-healed to: openai/gpt-4o

providers:
  anthropic: fail (current) — context length exceeded
  openai: ok (healed)
```

Self-heal only changes which model Craft uses. It does not edit your API keys or provider config files.

## Export a Diagnostics Report

Pass `--export` to print a structured JSON report instead of running self-heal. Useful for filing bug reports.

```bash
craft doctor --export > diagnostics.json
```

The report includes:

- Craft version and platform
- Config and state directory paths
- Current and saved model specs
- Per-provider status with error details
- The last 100 lines of your log file

```bash
craft doctor --export | jq '.providers[] | select(.status != "ok")'
```

## What the Ping Checks

Craft calls the provider's `list_models` endpoint with a short timeout (15 seconds). This is a real API call, so it catches expired keys, downed endpoints, and renamed models. It does not send any messages or use tokens beyond the request overhead.

If you want to check a specific model rather than relying on auto-detection, set it first with `-m`:

```bash
craft -m openai/gpt-4o doctor
```
