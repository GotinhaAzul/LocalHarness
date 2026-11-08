# Model setup

LocalHarness speaks the OpenAI chat-completions API. It does not download
models or start servers.

## Login and model selection

The harness opens even when no endpoint is reachable or no model is selected.
The TUI shows `not logged in` and blocks turns until `/login` succeeds:

```text
/login http://127.0.0.1:8080             auto-select the endpoint's only model
/login http://127.0.0.1:8080 qwen3       choose a model
/login http://host:8000 model sk-...     remote endpoint with a bearer token
/login                                   show the current settings and usage
```

`/login` probes `GET /v1/models`, rejects a model the endpoint does not list,
and saves `endpoint`, `model`, and `api_key` to the global config. Re-login to a
different endpoint without a model clears the old model and API key before
auto-selecting. `/model` opens a picker over every model the endpoint lists. Behind a gateway
(LiteLLM, OpenRouter, a vLLM router) that is every connected model; type to
filter, arrows to move, Enter to switch. `/model <name>` switches directly. The
switch lasts for the session; `/login` saves a default. If the endpoint cannot
list models, `/model` prints the error instead.

If the endpoint does not implement `/v1/models`, set the connection manually in
`~/.config/localharness/config.toml`, `--endpoint`/`--model`, or `LH_ENDPOINT`/
`LH_MODEL`.

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
lh --endpoint http://127.0.0.1:11434 --model qwen3
```

`GET /api/show` supplies the model context length. Tool calling needs a recent
Ollama and a tool-capable model.

Ollama's default context is small (2k to 4k tokens depending on version) and it
truncates longer prompts from the front without an error, which drops the
system prompt and tool definitions. The harness detects this from the usage
report, works inside the smaller window, and tells you; the fix is a larger
context on the server:

```sh
OLLAMA_CONTEXT_LENGTH=32768 ollama serve
```

or `PARAMETER num_ctx 32768` in a Modelfile.

## vLLM

```sh
vllm serve <model> --enable-auto-tool-choice --tool-call-parser <parser>
lh --endpoint http://127.0.0.1:8000 --model <model>
```

Without both flags the server rejects tool definitions; the harness reports the
flags to add.

## LM Studio

Enable the local server and load a model. Use its `/v1` base URL if it is not
`http://127.0.0.1:8080`.

## Tailnet or another machine

Point `[model] endpoint` at the host, with or without the `/v1` suffix:

```toml
[model]
endpoint = "http://desktopserver:8088/v1"
model = "spark26"       # or "" to auto-select when /v1/models has one entry

[model_profile.spark]
match = ["spark"]
context_window = 131072
tool_mode = "native"
reasoning_field = "reasoning_content"
```

The harness sends chat requests to `<endpoint>/v1/chat/completions` without
doubling an existing `/v1`, and keeps root probes (`/props`, `/tokenize`,
`/api/show`) on the server root. Non-local endpoints are allowed but print a
warning because prompts and tool output leave the machine; over a tailnet they
still leave the local host.

## Model profiles

Model quirks belong in TOML, not code:

```toml
[model_profile.gemma3]
context_window = 32768
strip_reasoning = true
tool_mode = "repair"        # native | repair | auto
```

- `tool_mode` decides what happens when a reply has no native tool call:
  `repair` and `auto` look for calls the model wrote as text and run them;
  `native` trusts the server's tool parser and treats such text as the answer.
  Recognized text formats: `<tool_call>` blocks (Hermes/Qwen JSON, Qwen3-Coder
  `<function=...><parameter=...>` XML, GLM `<arg_key>`/`<arg_value>`), bare
  `<function=name>` blocks, Mistral `[TOOL_CALLS]`, Llama `<|python_tag|>`,
  Gemma `tool_code` blocks, and fenced or bare JSON call objects. Fenced and
  bare JSON count only when they name an active tool and end the reply, so
  example JSON in an explanation is left alone. The first recovery in a turn
  prints a notice; if it keeps appearing, enable native tool calling on the
  server.
- `context_window` sets the prompt budget when probes are unavailable.
- `strip_reasoning` controls reasoning display/retention; reasoning is never
  sent back to the provider. `<think>`/`<thinking>` blocks that arrive in
  content (a server without a reasoning parser) are moved to reasoning, as is
  text before a lone `</think>` from templates that open the block in the
  prompt.
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

### Prefix cache and speed

The prompt is assembled append-only: system prompt, then history in order, with
the volatile state block last. Between compactions each call shares its whole
prefix with the previous one, so engines with prompt caching (llama.cpp by
default, vLLM with `--enable-prefix-caching`) only process the new tail.
Compaction rewrites history once and costs one full prompt pass.

The header shows generation speed and, when the engine reports it, the share of
the prompt served from cache (`31 tok/s · cache 94%`). The debug panel (`/debug`)
adds prompt processing time and time to first token for the last call. Sources:
`usage.prompt_tokens_details.cached_tokens` (vLLM, newer llama.cpp) or
llama.cpp's `timings`; time to first token is measured by the harness and works
with every engine. All of it is stored with each reply in the session file. A
low cache share between compactions means something else is evicting the
cache: another client on the same llama.cpp slot, or too few `--parallel`
slots.

## Troubleshooting

- **`tools` rejected (400)**: enable native tool calling (`--jinja`, vLLM flags).
- **`stream_options` rejected**: the harness retries once without it.
- **no first token**: the server may be loading the model. Raise
  `first_token_timeout_secs` or retry.
- **context too small**: raise the engine context or set
  `[model] context_window`; `/compact` forces a deterministic summary.
- **model not found**: `/model` lists installed models; switch with
  `/model <name>` or run `/login <endpoint> <model>`.
- **not logged in**: start the inference server, then `/login`; headless
  `--print` needs `--model` or a configured model.
- **checked edits show no diagnostics**: install the language server for the
  file type and open it inside a project with a marker (`Cargo.toml`,
  `pyproject.toml`, `package.json`, `go.mod`, `compile_commands.json`). Loose
  files and cold servers are skipped rather than failing an edit. A failed
  automatic start is remembered for the session; call `lsp` with
  `action="diagnostics"` once to warm the server and re-enable automatic checks.
- **reasoning shows nothing**: the model may not emit a reasoning field; set
  `reasoning_field` for that server.
- **"recovered N tool call(s) the model wrote as text"**: the server is not
  parsing tool calls. Start llama.cpp with `--jinja` (and a template that
  supports tools), use a tool-capable Ollama model, or start vLLM with
  `--enable-auto-tool-choice --tool-call-parser <parser>`. The harness keeps
  working meanwhile.
- **"server silently truncated the prompt"**: the engine's context is smaller
  than the prompt. Raise it (`OLLAMA_CONTEXT_LENGTH`, Modelfile `num_ctx`,
  llama.cpp `-c`). Until then the harness compacts to the observed window.
- **"output token limit"**: a reply was cut at `max_output_tokens` (default
  4096). A cut tool call is not run and the model is asked to send smaller
  calls; cut prose is continued. Raise `[model] max_output_tokens` if it
  repeats.
- **"repetition guard"**: the model fell into an exact loop and was stopped
  early. Lower `temperature`, or add a repetition penalty through
  `extra_body` (llama.cpp: `repeat_penalty`; vLLM: `repetition_penalty`).
