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
2. build         text to a document tree
3. guard         refuse a document too deep to score
4. select        find the article, drop the furniture
5. convert       article markup to Markdown
6. bound         cut what is over the ceiling and say so
```

Steps 2, 4 and 5 are separate functions rather than one block. That is not decoration: per-host extraction rules will hook between 2 and 4, telling the scorer directly which subtree is the article and which selectors are furniture, and the split is what makes that an addition rather than a rewrite. The record carries a `rules` field, `heuristic` until then, so a stored article always says how it was made.

### The parser, and why there are two of them in this binary

Metadata extraction reads a token stream and never holds more of the document than the token it is on. That is the right shape for reading tags, and it is the wrong shape for this: scoring a node for whether it is the article depends on its parent, its children and its siblings, so a tree is not an optimization here, it is the algorithm.

The tree is confined to this derived layer. The capture path is unchanged, and a run that never extracts an article never builds one.

`dom_smoothie` is a port of the Readability algorithm that Firefox reader mode and Pocket use, which is the most exercised answer to this problem that exists. `htmd` is a port of turndown.js and does the markup-to-Markdown step.

The two disagree about which version of `html5ever` they want, so **both are compiled in**. That is roughly a parser's worth of duplicate code in the binary and two implementations reading hostile input. It is accepted rather than solved: writing the Markdown emitter by hand against the tree that already exists would avoid it, but the part that emitter would own is escaping, and escaping attacker-controlled prose into a syntax where `*`, `_`, `#` and `[` are operators is precisely where a hand-rolled one goes wrong. The duplication disappears on its own when the two crates align.

### The title comes from elsewhere

The Readability algorithm removes the article's own heading from the content, because in its model the title is metadata, and the title it derives alongside is the raw `<title>`, site suffix and all.

This project already resolved that question. Metadata extraction picks a title across OpenGraph, schema.org and the document, with precedence rules that are written down and tested. So the title is handed in from there and written as the document's `#` heading, instead of this extractor forming a second opinion about what a page is called. A capture whose metadata found no title produces a Markdown file with no heading, which is honest.

## What is not an article

Most of the web is not. A listing page, a shop, a homepage and the shell of an application that renders itself in the browser are all captures with prose worth nothing, and writing an empty article record for each would fill the archive with files that say nothing.

The gate is the algorithm's own readability probe, run before the scoring pass. It is cheap and it discriminates: a listing of links, an empty application shell and a page of navigation all fail it, while an article passes. A capture that fails the probe gets no article files at all, which is an ordinary outcome and not an error.

Media types other than HTML never reach any of this, for the same reason they produce no metadata record.

## What a hostile page costs

The archive fetches addresses it was pointed at by other pages, so every ceiling here is a bound on what one document can spend, and reaching one is recorded rather than silently applied.

| ceiling | value | what it bounds |
|---|---|---|
| decoded document | 8 MiB | the tree that gets built at all |
| nesting depth | 256 | the scoring pass, see below |
| elements scored | 50 000 | the scoring pass on a wide document |
| Markdown kept | 1 MiB | the file that gets written |

### Nesting depth is the one that matters

The scoring pass is **cubic in how deeply the document nests**. Measured on this implementation, with a page that is nothing but nested `<div>` elements around one paragraph:

| depth | wall clock |
|---|---|
| 256 | 0.06 s |
| 500 | 0.33 s |
| 1 000 | 2.5 s |
| 2 000 | 20 s |
| 4 000 | 157 s |

Doubling the depth costs eight times the time, and the markup that buys it is tiny: the 157-second document is 40 KB on the wire. A page built this way is a denial of service against the capture, and none of the other ceilings catch it, because 4 000 nested elements is 4 000 elements and a few tens of kilobytes.

So depth is measured on the tree, which is linear and cheap, and a document over the ceiling is refused before the scoring pass ever runs. 256 is far past what real markup reaches, framework-generated pages included, and it holds the worst case to about sixty milliseconds.

The readability probe is not a substitute for this guard. It is fast and it happens to reject the pathological pages tried here, but it is a heuristic about whether a page reads like an article, not a bound on cost, and a page can be built to be both deeply nested and full of prose.

## The record

```json
{
  "extractor_version": 1,
  "rules": "heuristic",
  "word_count": 1240,
  "excerpt": "Bread is mostly patience.",
  "byline": "J. Writer",
  "truncated": []
}
```

`rules` names what produced the extraction. `byline` is what the algorithm found in the page's own markup and is not the resolved author in the metadata record: the two disagree often, and collapsing them would hide which one to look at when an attribution comes out wrong.

`extractor_version` is bumped when the meaning of a field or a rule that fills one changes, not when a field is added, on the same terms as the metadata record.

## What was deliberately left out

- **Per-host extraction rules.** A generic scorer works on markup that follows convention and fails on sites with a layout of their own, and the answer everyone converges on is a per-host override layer expressed as selectors. It is not built yet because the shape of the rule is not known yet: `body` and `strip` cover the cases imagined, and the fivefilters corpus grew to twenty directives against real sites. Committing to a format before three concrete sites have demanded one is inventing a schema. The seam is in place and the trigger is a real site coming out wrong, not a gap in the test corpus.
- **Plain text beside the Markdown.** Covered above.
- **Images pulled into the prose.** An article's images are already captured as subresources and addressed by content hash. Rewriting the Markdown to point at them is a question about how a reader resolves references, which belongs to the reader.
- **Pagination.** An article split across numbered pages is captured as the several pages it is served as. Stitching them is a per-site rule in every implementation that does it, so it waits for the rules layer.
- **Language-aware word counting.** The count splits on whitespace, which is wrong for languages that do not use it. It is a rough figure for sorting and filtering, not a measurement.

## Testing

Extraction quality varies by site and cannot be asserted exactly. Pinning the current output as expected would freeze today's behavior as correctness and break on every improvement, so the corpus asserts **bounds** instead: prose that must survive, furniture that must not, the heading hierarchy, and a range for the word count.

The fixtures are hand written, one file of markup and one of expectations, and they are minimal reproductions of shapes rather than saved pages. Real pages would carry a licence question into a public repository and hundreds of kilobytes per case.

This means the corpus does not discover the sites that need work; it cannot, since it only contains shapes someone already thought of. Discovery happens by running the tool. When a real page comes out wrong it is reduced by hand to the smallest markup that reproduces the failure, and that becomes a fixture. The corpus pins down what was found, so it cannot come back.
