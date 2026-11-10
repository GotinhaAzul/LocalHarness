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

## A tailnet or another machine

Point `[model] endpoint` at the host, with or without the `/v1` suffix:

```toml
[model]
endpoint = "http://model-host:8088/v1"
model = "my-model"      # or "" to auto-select when /v1/models has one entry

[model_profile.remote]
match = ["my-model"]
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

An engine can report three different context sizes, and they are not
interchangeable:

- **trained** — the model's trained maximum (`n_ctx_train`, LM Studio's
  `max_context_length`). Usually the largest number.
- **allocated** — what the server reserved (`--ctx-size`, `num_ctx`,
  `max_model_len`, `OLLAMA_CONTEXT_LENGTH`).
- **per-slot** — what one request may use when the server splits the
  allocation across parallel slots (`--parallel`, llama.cpp `/slots`).

The harness budgets against the smallest reported value:
`effective = min(trained, allocated, per_slot)`. With `llama-server --parallel
2 --ctx-size 8192`, `/props` reports 4096 per slot, so the effective window is
4096, not 8192. With Ollama, the trained maximum can be 131072 while the served
runner was allocated 4096; the effective window is 4096. When nothing is
reported the harness falls back to configuration, never to a fabricated value.
The one exception is Ollama's documented historical default: if Ollama is
detected with neither a loaded runner context nor a `num_ctx` parameter, the
harness assumes 4096 and says so (with the `OLLAMA_CONTEXT_LENGTH` fix) instead
of silently truncating prompts.

The session header records all four values (`trained`, `allocated`, `per_slot`,
`effective`) and the source of each probe, so a later session can explain why a
window was chosen. Older session files without that field load unchanged.

```
usable_prompt = effective_window - max_output_tokens - safety_margin
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

## Checking the engine: `lh doctor`

`lh doctor` fingerprints the configured engine from metadata endpoints only.
It never sends a chat completion and never loads a model. It prints the
manifest below; `lh doctor --json` prints the same manifest as stable, pretty
JSON. Fix lines name the exact server flag to change.

Probes are tried in this order and stop at the first match:

1. llama.cpp `GET /props`
2. Ollama `POST /api/show`
3. vLLM: `max_model_len` in the selected `/v1/models` entry and `GET /health`
4. SGLang `GET /server_info`
5. LM Studio `GET /api/v1/models`
6. KoboldCpp `GET /api/extra/version`
7. TabbyAPI `GET /v1/model`
8. `generic` — conservative defaults, with every failed probe listed

`POST /tokenize` is probed last to record whether exact token counts are
available. A manifest is cached per `(API base, model, build)` under the data
directory.

### Manifest fields

| Field | Meaning |
|---|---|
| `engine` | `llama.cpp`, `ollama`, `vllm`, `sglang`, `lmstudio`, `koboldcpp`, `tabbyapi`, or `generic`. |
| `build` | Server build/version string when reported. |
| `model` | The selected model id. |
| `context` | `trained`, `allocated`, `per_slot`, `effective`, plus `trained_source`, `allocated_source`, `per_slot_source`. `effective` is zero when nothing was reported. |
| `tool_dialect` | `native`, `repair`, `none`, or `unknown`, from server metadata (Ollama capabilities, llama.cpp chat template). |
| `tool_parser` | Parser name the server reports, if any. |
| `reasoning_field` | Streaming reasoning field the server reports, if any. |
| `structured_output` | `json_schema`, `grammar`, `none`, or `unknown`; unknown when the server does not report it. |
| `cache_mode` | Engine cache mode, if reported (for example TabbyAPI's `cache_mode`). |
| `slots` | Parallel slot count, if reported (llama.cpp `/props`). |
| `tokenizer_route` | `"/tokenize"` when exact tokenization works, else `null`. |
| `embeddings` / `rerank` | `true` only when the engine's metadata says so. |
| `probes_ok` | Metadata probes that answered. |
| `probes_failed` | Metadata probes that failed or did not identify an engine. |
| `warnings` | Context mismatch, Ollama default, unknown tool support, unrecognized engine. |
| `fixes` | Exact commands or config changes; `lh doctor --fix` prints them after the manifest. When the configured window is larger than the server's effective window, the first line names the flag that raises the server limit. |

### `--json` schema

```json
{
  "engine": "llama.cpp",
  "build": "b6040",
  "model": "qwen3",
  "context": {
    "trained": 131072,
    "allocated": 8192,
    "per_slot": 4096,
    "effective": 4096,
    "trained_source": "/v1/models meta.n_ctx_train",
    "allocated_source": "llama.cpp /props n_ctx × slots",
    "per_slot_source": "llama.cpp /props n_ctx (per slot)"
  },
  "tool_dialect": "native",
  "tool_parser": null,
  "reasoning_field": null,
  "structured_output": "unknown",
  "cache_mode": null,
  "slots": 2,
  "tokenizer_route": "/tokenize",
  "embeddings": false,
  "rerank": false,
  "probes_ok": ["llama.cpp /props", "POST /tokenize"],
  "probes_failed": [],
  "warnings": [
    "configured context_window 65536 exceeds the server's effective window 4096; prompts will be clamped to the server limit"
  ],
  "fixes": [
    "llama.cpp --ctx-size 65536 (or raise the profile's context_window to the server limit)",
    "llama.cpp: llama-server -m <model> --jinja --ctx-size 65536 --port 8080"
  ]
}
```

Optional values are `null` rather than omitted, and every field is always
present, so the JSON can be consumed by scripts. `context.effective == 0` means
the server reported nothing; the harness falls back to the configured
`context_window`.

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
