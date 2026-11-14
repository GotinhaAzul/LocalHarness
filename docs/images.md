# Images

Screenshots carry layout and colour that words lose: "this button is
misaligned", "match this design", "the error is in this dialog". LocalHarness
accepts PNG, JPEG, and WebP images in two ways:

- **`@shot.png` in a prompt** attaches the image to your message.
- **The `read` tool** opens an image file when the model asks for it, so the
  model can look at screenshots or rendered output on its own.

Nothing leaves your machine unless you allow it: images go only to loopback or
private-network endpoints by default.

## How an image reaches the model

| Active model | What happens |
|---|---|
| Has vision input | The image is sent as an OpenAI `image_url` content part. |
| Text only, `[vision] model` set | That model describes the image once; the description is attached as text, labelled `described_by`. |
| Text only, no `[vision] model` | The image is refused with a message naming both fixes. |

A model has vision input when its model profile says `vision = true`. When the
profile does not say, LocalHarness asks the server: llama.cpp reports
`modalities.vision` in `/props` when it was started with a vision projector
(`--mmproj`). Gateways that do not expose `/props` need the profile setting.

```toml
[model_profile.qwen-vl]
match = ["qwen2.5-vl", "qwen3-vl"]
vision = true

# Optional: describe images for text-only models with a local vision model.
[vision]
model = "qwen2.5-vl-7b"
# endpoint = "http://127.0.0.1:8081/v1"   # empty uses [model] endpoint
# api_key = ""
```

If you switch to a text-only model mid-session, images already in the history
are replaced by a placeholder rather than sent.

## Safety and privacy

- **Checked before reading.** The size is checked from file metadata, and the
  format from the first bytes (magic numbers, not the extension), before the
  body is read. The default cap is 5 MB (`[vision] max_bytes`).
- **Metadata stripped.** EXIF, XMP, IPTC, comments, and PNG text chunks are
  removed before the image is stored or sent; colour profiles are kept. Pass
  `--keep-exif` (or `[vision] keep_exif = true`) to send the original bytes.
  Stripping drops the EXIF orientation tag, so a phone photo may appear
  rotated.
- **Local by default.** Images are sent only to `localhost`, loopback, RFC 1918,
  link-local, and IPv6 unique-local addresses. Any other endpoint, including a
  hostname on your LAN or VPN, needs `--allow-remote-images` or
  `[vision] allow_remote = true`. The same rule applies to the `[vision]`
  describer endpoint.
- **No bytes in the transcript.** The session records an artifact id, the
  dimensions, and the size; the image itself is stored next to the session's
  other artifacts (`img-N.png`) and follows the same retention. With
  `--no-session` it is kept in memory only.

## Cost

Vision encoders spend roughly one token per 28×28 pixel patch. The context
budget counts each image at that estimate (between 64 and 1600 tokens), so a
large screenshot can use a noticeable share of a small window.
