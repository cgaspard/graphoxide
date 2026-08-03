# Transcription

Graphoxide does not transcribe audio or video inside the native binary. Produce a UTF-8 transcript with an appropriate local or approved service, keep it beside the source media, and extract the transcript as a document:

```bash
graphoxide extract path/to/corpus
graphoxide audit path/to/corpus --strict
```

Retain timestamps in the transcript so graph evidence can be traced back to the recording.
