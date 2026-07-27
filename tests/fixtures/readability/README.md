# The readability corpus

One `<name>.html` and one `<name>.expected.json` per case. `tests/readability_corpus.rs` walks this directory, so adding a case is adding two files and touching no code.

The pages are written by hand and are minimal reproductions of shapes, not saved pages. Saving real pages would carry a licence question into a public repository and hundreds of kilobytes per case.

Nothing here asserts the output exactly. Extraction quality varies by site, and pinning today's Markdown as expected would freeze current behavior as correctness and break on every improvement. What is asserted is bounds: prose that must survive, furniture that must not, the heading hierarchy, and a range for the word count.

## The expectation file

```json
{
  "why": "what this case is here to prove",
  "is_article": true,
  "title": "the title the metadata extractor would have resolved, or null",
  "must_contain": ["prose that has to survive"],
  "must_not_contain": ["furniture that must not"],
  "heading_levels": [1, 2, 2],
  "word_count": { "min": 80, "max": 200 }
}
```

`is_article: false` means the extractor must produce nothing, and every other field is then unused. `heading_levels` and `word_count` are optional.

## Where a case comes from

Not from imagination, past the handful of shapes that seeded this. A hand-written corpus only contains what someone already thought of, so it does not discover the sites that need work; running the tool does. When a real page comes out wrong it is reduced by hand to the smallest markup that reproduces the failure, and that becomes a case here. The corpus pins down what was found, so it cannot come back.
