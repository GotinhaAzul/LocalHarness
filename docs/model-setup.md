# Model setup

LocalHarness speaks the OpenAI chat-completions API. It does not download
models or start servers.

## llama.cpp

Tool calling requires the Jinja chat template:

```sh
llama-server -m model.gguf --ctx-size 65536 --jinja --port 8080
```

If tools are rejected, the harness names `--jinja` in the error. It does not
silently drop tools and become a chat client.

Optional probes: `GET /props` (real context window, chat template) and
`POST /tokenize` (exact token counts). `--ctx-size` must be at least as large as
the model profile's `context_window`, or the provider rejects long prompts.

## Ollama

```sh
ollama serve
ollama pull qwen3
lh --model qwen3
```

`GET /api/show` supplies the model context length. Tool calling needs a recent
Ollama and a tool-capable model.

## vLLM

```sh
vllm serve <model> --enable-auto-tool-choice --tool-call-parser <parser>
```

Without both flags the server rejects tool definitions; the harness reports the
flags to add.

## LM Studio

Enable the local server and load a model. Use its `/v1` base URL if it is not
`http://127.0.0.1:8080`.

## Model profiles

Model quirks belong in TOML, not code:

```toml
[model_profile.gemma3]
context_window = 32768
strip_reasoning = true
tool_mode = "repair"        # native | repair | auto
```

- `context_window` sets the prompt budget when probes are unavailable.
- `strip_reasoning` controls reasoning display/retention; reasoning is never
  sent back to the provider.
- `reasoning_field` names the streaming field (`reasoning_content` by default,
  falling back to `reasoning`).
- `stop` adds stop sequences.
- `extra_body` merges server-specific fields into every request, for example
  `chat_template_kwargs`.
- `parallel_tool_calls` is recorded for compatibility; tools run sequentially.

## Context and token accounting

```
usable_prompt = context_window - max_output_tokens - safety_margin
```

The safety margin is 5% of the window, minimum 512 tokens. Compaction starts at
80% of the usable prompt, or when the provider returns a context-length error.
Prompt token counts from `usage` correct the character-based estimate after each
call; `/tokenize` calibrates it when available.

## Troubleshooting

- **`tools` rejected (400)**: enable native tool calling (`--jinja`, vLLM flags).
- **`stream_options` rejected**: the harness retries once without it.
- **no first token**: the server may be loading the model. Raise
  `first_token_timeout_secs` or retry.
- **context too small**: raise the engine context or set
  `[model] context_window`; `/compact` forces a deterministic summary.
- **model not found**: `GET /v1/models` lists installed models; pass `--model`.
- **reasoning shows nothing**: the model may not emit a reasoning field; set
  `reasoning_field` for that server.
