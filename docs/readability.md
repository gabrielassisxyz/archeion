# Readability extraction

How the prose of a captured page is separated from the furniture around it, what the result looks like on disk, and what a hostile page costs on the way.

## What this produces

Beside a capture that turned out to be an article there are two more files:

```
items/<host>/<item-id>/captures/<capture-id>.article.md     the prose, as Markdown
items/<host>/<item-id>/captures/<capture-id>.article.json   what is known about it
```

Both are derived. The response body in `blobs/` is the only thing that cannot be recovered, and both of these can be deleted and rebuilt from it without fetching anything. That is the same arrangement `<capture-id>.metadata.json` already has, and the reason is in [`storage-model.md`](storage-model.md).

## Why Markdown

The consumer of this artifact is a reading surface that shows pages from a thousand different origins. What it needs is prose in a **uniform vocabulary**, so that one stylesheet renders every article the same way regardless of which site it came from.

Cleaned HTML would not give that. It preserves whatever structure and class names the origin used, so a page from one site and a page from another stay visibly different documents. Markdown is a closed vocabulary: heading, paragraph, list, quote, code, link, emphasis. Everything else is dropped on the way in, which is exactly the normalization wanted.

It also survives the tool. A directory of Markdown files is readable in any editor, on any machine, in thirty years, with no program that understands this archive.

**Plain text is not stored beside it.** It is derivable from the Markdown by removing markup, an index has to tokenize the text anyway, and a second file would be a second thing to keep consistent for no reader that exists. Generating one later is a sweep over the `.md` files with no refetch, which is the property the derived layer was built for.

## The pipeline

```
1. decode        response bytes to text            metadata/decode.rs, shared
2. weigh         refuse markup too expensive to parse
3. build         text to a document tree
4. guard         refuse a document too deep to score
5. select        find the article, drop the furniture
6. convert       article markup to Markdown
7. bound         cut what is over the ceiling and say so
```

The order of the first four is the design, not an arrangement. Each guard has to run before the step it protects, and each would be useless after it.

Steps 3, 5 and 6 are separate functions rather than one block. That is not decoration: per-host extraction rules will hook between 2 and 4, telling the scorer directly which subtree is the article and which selectors are furniture, and the split is what makes that an addition rather than a rewrite. The record carries a `rules` field, `heuristic` until then, so a stored article always says how it was made.

### The parser, and why there are two of them in this binary

Metadata extraction reads a token stream and never holds more of the document than the token it is on. That is the right shape for reading tags, and it is the wrong shape for this: scoring a node for whether it is the article depends on its parent, its children and its siblings, so a tree is not an optimization here, it is the algorithm.

The tree is confined to this derived layer. The capture path is unchanged, and a run that never extracts an article never builds one.

`dom_smoothie` is a port of the Readability algorithm that Firefox reader mode and Pocket use, which is the most exercised answer to this problem that exists. `htmd` is a port of turndown.js and does the markup-to-Markdown step.

The two disagree about which version of `html5ever` they want, so **both are compiled in**. That is roughly a parser's worth of duplicate code in the binary and two implementations reading hostile input.

Two ways out were measured and rejected. `dom_query`, which arrives with `dom_smoothie`, ships a Markdown serializer of its own, reachable both directly and through `TextMode::Markdown`; taking it would remove `htmd`, the second `html5ever` and a re-parse of the article markup. It escapes every sentence-ending period, so `patience.` is written `patience\.`, which ruins the one property that makes Markdown worth choosing. Writing the emitter by hand against the tree that already exists would also avoid the duplication, but the part that emitter would own is escaping, and escaping attacker-controlled prose into a syntax where `*`, `_`, `#` and `[` are operators is precisely where a hand-rolled one goes wrong.

The duplication disappears on its own when the two crates align on a parser version.

### The title comes from elsewhere

The Readability algorithm removes the article's own heading from the content, because in its model the title is metadata, and the title it derives alongside is the raw `<title>`, site suffix and all.

This project already resolved that question. Metadata extraction picks a title across OpenGraph, schema.org and the document, with precedence rules that are written down and tested. So the title is handed in from there and written as the document's `#` heading, instead of this extractor forming a second opinion about what a page is called. A capture whose metadata found no title produces a Markdown file with no heading, which is honest.

**The title is the one string here that a page controls and that does not arrive through the converter**, and a heading is a line: a newline inside it ends the heading, and everything after becomes document structure. A line break in an attribute value is legal, so a page serving `content="Bread` newline newline `## Security notice ..."` could write its own headings, links and paragraphs into the archived article, indistinguishable from extracted prose and attributed by the record beside it to the heuristic. Whitespace is therefore collapsed to single spaces, and the result is escaped by the same converter the body goes through rather than by rules written here, because two escapers are two sets of rules to keep in agreement.

## What is not an article

Most of the web is not. A listing page, a shop, a homepage and the shell of an application that renders itself in the browser are all captures with prose worth nothing, and writing an empty article record for each would fill the archive with files that say nothing.

The gate is the algorithm's own readability probe, run before the scoring pass. It is cheap and it discriminates: a listing of links, an empty application shell and a page of navigation all fail it, while an article passes. A capture that fails the probe gets no article files at all, which is an ordinary outcome and not an error.

Media types other than HTML never reach any of this, for the same reason they produce no metadata record.

## What a hostile page costs

The archive fetches addresses it was pointed at by other pages. Two of the libraries it hands them to have costs that grow faster than the input, so a page a few hundred kilobytes long can be built to cost minutes, and every ceiling here exists because a measurement said so.

| ceiling | value | what it bounds | where it lands |
|---|---|---|---|
| decoded document | 1 MiB | how much work any of the below can be asked to do | the run report |
| open elements | 2 048 | the parse | the run report |
| nesting depth | 256 | the scoring pass | the run report |
| elements scored | 50 000 | the scoring pass on a wide document | the run report |
| Markdown kept | 1 MiB | the file that gets written | the stored record |

Only the last is recorded in the archive, as `truncated`, because it is the only one where an article still exists to describe. The other four produce no article at all, so what they leave is an entry in the run's report naming the URL and the ceiling. That is the honest state of it: a page refused for cost and a page that simply was not an article look the same on disk.

### Nesting depth, and what the first measurement of it got wrong

The scoring pass grows sharply with nesting depth. Measured on a page that is nothing but nested `<div>` elements around one paragraph:

| depth | wall clock |
|---|---|
| 256 | 0.06 s |
| 500 | 0.33 s |
| 1 000 | 2.5 s |
| 2 000 | 20 s |
| 4 000 | 157 s |

Doubling the depth costs eight times the time, and the markup that buys it is tiny: the 157-second document is 40 KB on the wire.

**That table alone does not support the conclusion drawn from it.** Every row varies element count and depth together, since a chain of `n` nested elements is `n` deep and has `n` elements, so it cannot tell a cost cubic in depth from one that is merely quadratic in depth and linear in element count. Separating them:

| shape | elements | depth | wall clock |
|---|---|---|---|
| 10 chains, 253 deep | 2 530 | 253 | 0.07 s |
| 160 chains, 253 deep | 40 480 | 253 | 0.23 s |
| 1 000 chains, 4 deep | 4 000 | 4 | 0.02 s |
| 16 chains, 253 deep | 4 048 | 253 | 0.07 s |

Depth is what costs; element count barely moves the result. A document at both ceilings at once, which is the worst case the guards allow, stays in that range.

So depth is measured on the tree, which is linear and cheap, and a document over the ceiling is refused before the scoring pass ever runs. The depth allowance for the markup itself is three short of the constant, because the parser implies `<html>` and `<body>` around it and depth is a property of the tree.

The readability probe is not a substitute for this guard. It is fast and it happens to reject the pathological pages tried here, but it is a heuristic about whether a page reads like an article, not a bound on cost, and a page can be built to be both deeply nested and full of prose.

### Open elements, which is the guard that has to come first

The tree parser rescans its stack of open elements for every end tag that has to search a scope. Markup that opens elements and never closes them therefore costs time quadratic in its own size:

| document | wall clock |
|---|---|
| 132 KB | 0.3 s |
| 264 KB | 1.2 s |
| 528 KB | 18.3 s |
| 1 MiB, the ceiling | about 72 s, extrapolated |

Doubling the input quadruples the time. This is the reason the byte ceiling is 1 MiB rather than the 8 MiB it started at, and the reason 1 MiB is still not enough on its own.

Neither of the other guards helps. The depth ceiling is measured on a tree, and by the time there is a tree the cost has been paid; the element ceiling is applied by the scorer, one step later still. So the count is taken on the bytes, before anything is parsed, in `markup_scan`. A raw scan reads 8 MB in 3 ms where the token-stream parser takes 4 s on the same input, and it stops at the first element past the ceiling.

Reading bytes rather than parsing them means the scan has to know a few things or it will refuse ordinary pages: void elements never close and would otherwise accumulate, so a gallery of images would look like unbalanced markup; `<` is an operator in every language a page embeds, so script and style bodies are skipped; attribute values hold `<` and `>` in ordinary prose; and a comment ends at `-->` and not at the first `>` inside it. Where it is still wrong it is wrong upward, as with prose that was never escaped, so it refuses rather than admits.

### The ceilings are expected to come down

They are set where a hostile page is certainly refused, not where a real page is certainly kept, and the distance between those two is unknown. So every article records what it actually measured, in `cost`, for every page and not only for the refused ones: a count of refusals says whether a ceiling is firing, and only the values real articles reach can say whether a lower ceiling would start refusing them. After enough real captures, the ceilings move against that distribution rather than against a guess.

## The record

```json
{
  "markdown_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "extractor_version": 1,
  "rules": "heuristic",
  "word_count": 1240,
  "excerpt": "Bread is mostly patience.",
  "byline": "J. Writer",
  "cost": {
    "document_bytes": 48213,
    "peak_open_elements": 42
  }
}
```

`rules` names what produced the extraction. `byline` is what the algorithm found in the page's own markup and is not the resolved author in the metadata record: the two disagree often, and collapsing them would hide which one to look at when an attribution comes out wrong. `word_count` counts the prose and not the heading, which is a title the metadata record already holds. `truncated` is absent when nothing was cut, which is the ordinary case.

`markdown_sha256` is the address of the document beside it, and it is what makes the pair safe to rewrite. Ordering alone is enough only the first time: writing over an existing pair and stopping between the two files leaves new prose beside an old record, both present, both parsing, and every field describing something that is no longer there. A reader that finds the two disagreeing reports no article, because the response the article was derived from is still in the archive and the pass that re-extracts will simply redo it.

`extractor_version` is bumped when the meaning of a field or a rule that fills one changes, not when a field is added, on the same terms as the metadata record.

## What was deliberately left out

- **Per-host extraction rules.** A generic scorer works on markup that follows convention and fails on sites with a layout of their own, and the answer everyone converges on is a per-host override layer expressed as selectors. It is not built yet because the shape of the rule is not known yet: `body` and `strip` cover the cases imagined, and the fivefilters corpus grew to twenty directives against real sites. Committing to a format before three concrete sites have demanded one is inventing a schema. The seam is in place and the trigger is a real site coming out wrong, not a gap in the test corpus.
- **Plain text beside the Markdown.** Covered above.
- **Images pulled into the prose.** An article's images are already captured as subresources and addressed by content hash. Rewriting the Markdown to point at them is a question about how a reader resolves references, which belongs to the reader.
- **Pagination.** An article split across numbered pages is captured as the several pages it is served as. Stitching them is a per-site rule in every implementation that does it, so it waits for the rules layer.
- **Language-aware word counting.** The count splits on whitespace, which is wrong for languages that do not use it. It is a rough figure for sorting and filtering, not a measurement.
- **Removing an article.** There is a way to write a pair and no way to un-write one, so an extractor that later decides a capture is not an article leaves the previous pair in place. Nothing needs it until a pass over an existing archive exists, and that pass is the caller that will define what removal should mean.
- **A ceiling on wall clock.** Every guard here bounds a shape that was measured. None of them bounds a shape that was not, and a per-document time limit is the only thing that would. It is not built because a thread cannot be stopped from outside in this language, so such a limit would bound how long a capture waits without bounding what it spends, and a host serving many such pages would saturate the machine either way.
- **A guard on the metadata path.** The quadratic parse described above is a property of reading hostile markup, not of this module: metadata extraction takes 0.6 s on the same input where this took 18 s. It is bounded there, but by a memory ceiling that happens to cut the blowup short rather than by anything aimed at it. Giving it the same scan is its own change.

## Testing

Extraction quality varies by site and cannot be asserted exactly. Pinning the current output as expected would freeze today's behavior as correctness and break on every improvement, so the corpus asserts **bounds** instead: prose that must survive, furniture that must not, the heading hierarchy, and a range for the word count.

The fixtures are hand written, one file of markup and one of expectations, and they are minimal reproductions of shapes rather than saved pages. Real pages would carry a licence question into a public repository and hundreds of kilobytes per case.

This means the corpus does not discover the sites that need work; it cannot, since it only contains shapes someone already thought of. Discovery happens by running the tool. When a real page comes out wrong it is reduced by hand to the smallest markup that reproduces the failure, and that becomes a fixture. The corpus pins down what was found, so it cannot come back.
