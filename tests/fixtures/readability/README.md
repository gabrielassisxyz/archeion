# The readability corpus

One `<name>.html` and one `<name>.expected.json` per case. `tests/readability_corpus.rs` walks this directory, so adding a case is adding two files and touching no code.

The pages are written by hand and are minimal reproductions of shapes, not saved pages. Saving real pages would carry a licence question into a public repository and hundreds of kilobytes per case.

Nothing here asserts the output exactly. Extraction quality varies by site, and pinning today's Markdown as expected would freeze current behavior as correctness and break on every improvement. What is asserted is bounds: prose that must survive, furniture that must not, the heading hierarchy, and a range for the word count.

## The expectation file

```json
{
  "why": "what this case is here to prove",
  "outcome": "article",
  "title": "the title the metadata extractor would have resolved, or null",
  "must_contain": ["prose that has to survive"],
  "must_not_contain": ["furniture that must not"],
  "heading_levels": [1, 2, 2],
  "word_count": { "min": 80, "max": 200 },
  "rules": { "why": "what this host had to be told", "strip": [".appeal"] }
}
```

`outcome` is one of three, because the extractor has three answers and two of them are not an article:

- `article`, and every other field asserts something about it.
- `refused`, meaning prose came out and a rule here turned it down. It leaves a record beside the capture for a later review to answer, so the other fields are unused.
- `nothing`, meaning the page held nothing worth reading and is passed over in silence. The other fields are unused.

The last two are separate cases rather than one "not an article" because they leave different things behind. A fixture that only said "not an article" would keep passing if a refusal quietly became silence, and the queue of pages somebody is meant to review would empty without anyone noticing.

`heading_levels` and `word_count` are optional.

`rules` is what this page's host has been told, in the shape `extraction-rules.json` declares it, and the page is served from `<name>.example.com` so the rule keys on it. A case that carries one asserts twice: the bounds above hold with the rule, and they do not hold without it. The second half is checked separately, and it is what keeps a rule from staying here after it stopped being needed.

## Where a case comes from

Not from imagination, past the handful of shapes that seeded this. A hand-written corpus only contains what someone already thought of, so it does not discover the sites that need work; running the tool does. When a real page comes out wrong it is reduced by hand to the smallest markup that reproduces the failure, and that becomes a case here. The corpus pins down what was found, so it cannot come back.
