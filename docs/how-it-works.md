# How Graphoxide works

Graphoxide's default extraction pipeline is deterministic and offline. Code files are not sent to the LLM semantic extractor: language parsers produce their symbols and relationships directly.
When a corpus contains only code files, Pass 3 is skipped entirely because no semantic-provider
work is needed.

Optional semantic ingestion is reserved for docs, papers, images, and transcripts. Those inputs
remain separate from deterministic code extraction, so code-only graph builds never require an API
key or network access.
