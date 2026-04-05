# lore-git-md-helper

An AI-powered toolkit for the [Git mailing list](https://lore.kernel.org/git/).

Converts raw mbox emails into clean, AI-friendly Markdown, then
summarizes threads and generates daily, weekly, and monthly digests
so maintainers and contributors can stay informed without reading
every message.

**Browse the output at [git-digestif.github.io](https://git-digestif.github.io/).**
The full Markdown corpus lives at
[git-digestif/lore-git-md](https://github.com/git-digestif/lore-git-md).

## What it does

- **mbox2md** -- converts mbox/eml emails to structured Markdown
  preserving diffs, nested quoting, and ASCII art
- **update-lore-git-md** -- batch-imports emails from a
  [lore-git](https://lore.kernel.org/) source repo into a date-keyed
  Markdown repository with thread symlinks and Message-ID notes
- **digestive** -- runs a four-call AI summarization pipeline per email
  (human summary, AI summary, thread human, thread AI) and rolls up
  into daily, weekly, and monthly digests
- **lore-rag** -- local RAG (retrieval-augmented generation) Q&A over
  the converted Markdown corpus using FTS5 search
- **email-surgery** -- safely moves or removes misdated/spam emails,
  updating all thread metadata and notes

## Quick start

```sh
cargo build --release
```

Binaries land in `target/release/`.

## Running tests

```sh
cargo test --features test-support
```

The `test-support` feature enables integration tests for the binary
crates that need access to shared test helpers.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
