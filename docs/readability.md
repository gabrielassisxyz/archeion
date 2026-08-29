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
5. narrow        apply what this host has been told, if anything
6. select        find the article, drop the furniture
7. convert       article markup to Markdown
8. bound         cut what is over the ceiling and say so
9. judge         refuse a sliver of a page that mostly said something else
```

The order of the first four is the design, not an arrangement. Each guard has to run before the step it protects, and each would be useless after it. The last step is the opposite case: it weighs the extraction against the page it came out of, so it is the one thing here that can only run once both exist.

Step 5 is where a host's own rules apply, and it sits where it does because it is the last moment the document exists in one piece and the first moment there is a tree to select in. The record carries a `rules` field naming what produced the extraction, so a stored article always says whether it was worked out or told.

A response that already is the prose runs none of it. Steps 2 through 6 all exist to find an article inside markup, and there is no markup and nothing to find: the site published the separation this pipeline reconstructs. What it runs instead is in [the document a site already published](#the-document-a-site-already-published), which is the same last three steps and its own guards in front of them.

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

### Links and images are not trusted syntax

A link destination is the one string in an article a reader may act on. The schemes an archived document may still point at are therefore `http`, `https` and `mailto`, plus a fragment, which addresses this document and has no scheme to hide. A relative destination is resolved against the address the response came from, so the stored article carries the same absolute spelling export compares later. Everything else loses the link and keeps its text. Image sources use the same destination policy, so `data:image` is not a special case with its own rule.

An image description is reduced to something that can only be a description. Both routes flatten it into an attribute and then into `![...](...)`, where the converter escapes almost nothing. Four characters leave: `]`, which ends the description, `[`, which opens another, a trailing `\`, which escapes the closing bracket that should have ended it, and a line break, which ends the line the construct sits on. Whitespace collapses to single spaces, for the reason it collapses in the title. Image and link titles are the same kind of string and are reduced the same way.

The HTML path applies this while `href`, `src`, `alt` and `title` are still attributes, by configuring the `htmd` conversion for `a` and `img`. Doing it after conversion would be pattern matching in a syntax where `*`, `_`, `#`, `[` and `]` are operators, which is the job the converter exists to own. The served-Markdown path has a different mechanism, a CommonMark event stream before generated markup exists, but it calls the same policy.

`EXTRACTOR_VERSION` is 3 for this change. Existing `heuristic` and `site:<host>` records can already contain headings or destinations injected through page attributes, so their Markdown did not merely miss future information. It said something under weaker rules. A repass must therefore treat them as stale and rebuild the derived article from the stored response before replacing it.

### An image is written at the largest size the page offered

A page that lists one picture at several sizes is offering a choice a browser makes against a viewport. An archive has no viewport, and what it has instead is a reader who wants the picture, so the widest candidate of a `srcset` is the address written into the article and `src` is what an image without candidates falls back to. Taking `src` first is how a note ends up naming a thumbnail while the archive holds the full size file beside it.

The preference is abandoned rather than defended. A candidate is used only when it is already absolute, because the layer below absolutizes what it can reach and a candidate still relative afterwards is one it could not, so resolving it here against the response address would invent an origin the page never named. A candidate the destination policy above refuses, an inline placeholder being the ordinary case, falls back to `src` as well: a `srcset` holding nothing usable must not cost the picture that `src` was pointing at all along.

Reading candidates first also steps around a repair in the layer below, which copies an attribute whose value merely contains an image extension over `src` or `srcset`. A platform that describes its pictures with a JSON descriptor in a data attribute has that descriptor land in `src`, where it becomes an address resolved against the page's own path that no server answers. Stepping around it is all this does: where that repair overwrote `srcset` instead, nothing here recovers the original.

### An anchor around a picture is one link

A link is inline syntax and an anchor is allowed to wrap block content, a picture inside its own container being the ordinary way a platform writes a linked image. Emitted as it stands, the block ends the paragraph the opening bracket started, and the note carries a bare `[`, the content, and then `](address)` as literal characters that no CommonMark reader sees as a link. The export finds the destinations it rewrites into paths between notes by reading the Markdown, so a destination spelled that way is one a collection cannot use to link itself together.

An anchor's children are therefore reduced to one line. Trimming is what the common case needs; anything still spanning lines has no inline spelling at all and has its whitespace collapsed, which keeps the text and the destination and loses only the arrangement inside something that was one link to begin with. One line rather than the absence of a blank line, because a list is joined by single newlines and a list item interrupts a paragraph, and a fenced code block interrupts one too and would leave everything after that link inside an unterminated block. Whitespace at either edge moves outside the link rather than being dropped with the rest, or an anchor padded by the markup around it runs two words together.

`EXTRACTOR_VERSION` is 4 for the two rules above. A record written under 3 names a different address for the same image and spells a linked picture as characters that are not a link, so it does not merely miss something added since: an export built from it differs from one built from a rebuild, and a repass has to treat it as stale.

### An embedded document leaves a link where it was

An embedded video, track or episode is ordinarily a container with no text of its own, `<iframe src="...">` and nothing else, and a container with no text is exactly what the scoring pass is built to drop. What survived into the archive before this was the sentence that introduced the embed, pointing at nothing that followed it.

The scoring library's own answer to this is a short, hardcoded list of video domains: an `iframe` naming one of them survives its cleaning pass unaided, and every other `iframe`, `object` and `embed` is removed outright regardless of what it points at. `www.youtube-nocookie.com` is on that list. `open.spotify.com` and `embed.podcasts.apple.com`, both seen in the corpus this rule was measured against, are not, and neither is any host a page might embed that nobody has read yet.

So the rule here runs before the scoring pass rather than inside it, on the tree this project already owns before handing it to the scorer: every `iframe[src]` whose address resolves to a destination the archive's own link policy keeps is rewritten in place into an anchor carrying that address, labelled with its host. An anchor is a shape the scoring pass already knows to keep, on the same terms as a video a page wrote as a plain link, so the existing anchor handling produces the Markdown from there with nothing further added: `[youtube-nocookie.com](https://www.youtube-nocookie.com/embed/rJ6RZ2YzaLc)`.

The label is the host and not the address, because the address is what the destination already says. Measured on the corpus this rule was built against: not one of its 176 iframes carried a `title` attribute, so there is no author-written text to reuse, and a bare address in the label would turn a note into a list of them. The host is read with its `www.` prefix removed, the same way a reader's own browser already hides it, using the one-step rule that a bare `www.com` is a registrable name and not a prefix of one.

**The rule is not keyed on a host.** Nothing here consults the scoring library's whitelist or one of its own: an `iframe` is rewritten and linked on the strength of its element name and a resolvable `src` alone, so a platform absent from every corpus this project has read is covered on the same terms as one already seen.

**What this covers, and what it knowingly does not.** Covered: any `iframe` whose `src` resolves to a destination this archive's link policy would keep for an ordinary anchor, which includes a relative `src` resolved against the page's own address. Not covered, deliberately: `<video>`, `<audio>`, `<embed>` and `<object>`, which had zero occurrences across the corpus this rule was measured against and therefore no evidence to write a rule from; and a platform that writes an embed's address only into a data attribute and ships no `iframe` at all, which the same corpus also had none of. Either arriving in a real capture is a new measurement and a new rule, not an extension of this one.

An `iframe` with no `src`, or a `src` the link policy refuses, is left exactly as it was: the scoring library's own cleaning pass then decides its fate on its own terms, which is removal for anything not on its whitelist, never an empty link.

`EXTRACTOR_VERSION` is 5 for this. A record written under 4 simply lost the element, so its absence of a link is not evidence there was nothing to link: the response may hold an embed the article never mentioned, which is precisely the defect this rule exists to fix.

## What is not an article

Most of the web is not. A listing page, a shop, a homepage and the shell of an application that renders itself in the browser are all captures with prose worth nothing, and writing an empty article record for each would fill the archive with files that say nothing.

The first gate is the algorithm's own readability probe, run before the scoring pass. It is cheap and it discriminates: a listing of links, an empty application shell and a page of navigation all fail it, while an article passes. A capture that fails the probe gets no article document, which is an ordinary outcome and not an error.

Media types other than HTML and Markdown never reach any of this, for the same reason they produce no metadata record.

A page that is HTML and still not an article gets a mark of its own, `<capture-id>.article-not-found.json`, when a capture or repass writes the derived layer. That mark exists because a later pass would otherwise spend the same parse again to reach the same answer. It is narrower than absence: absence still means no extractor has answered yet, a non-HTML response had nothing to read, or the derived layer was deliberately removed.

### The sliver rule, and the page the probe lets through

A site's own front page defeats the probe. It carries a tagline, a description and a footer blurb around its list of links, which is more prose than an imagined listing has, so the probe admits it and the scorer then returns whichever of those blocks scored best. Capturing sixty pages of one site produced forty-eight articles from forty-six posts, and one of the extras was a front page stored as forty-four words of boilerplate, against a median of about two thousand for the real articles beside it.

So a second gate runs after the extraction rather than before it: an extraction is refused when it is **both** under 300 characters and under a quarter of the text its page holds.

**Link density, the obvious instrument, cannot see this.** A listing is made of links, and that is what the underlying algorithm already scores on, so the reasonable expectation is that a threshold somewhere in its configuration would refuse the page. It would not. The list is dropped as furniture before the article is formed, and what would be stored is genuine prose carrying no links at all. Nor is either number reachable through that configuration: `readable_min_score` and `readable_min_content_length` weigh text length alone, and `char_threshold` refuses nothing, because the grab loop falls back to its best attempt when no attempt reaches it.

**Characters and not words.** Words are whitespace-separated only in some languages. Counting them scores a Chinese or Japanese article at its paragraph count, three or eight, while the navigation and footer around it keep one token per element exactly as they would in English, so a rule weighing words refuses those pages however long they are, and refuses them harder the more they say. Both sides are therefore counted in characters, whitespace excluded so that indented markup does not read as more text than the same page minified.

Characters are not neutral either, only usable. Three hundred of them is around fifty words of English and several times that in Chinese, so the floor is more generous to a language that writes densely. It errs toward keeping, which is the direction an archive should err in, and the corpus holds a page in such a language so that the claim is checked rather than assumed.

**Neither number would do on its own**, and each covers the other's mistake:

| page | characters | share of its page | kept by |
|---|---|---|---|
| a site's front page | 137 | 0.12 | nothing, which is the point |
| a short post on a plain page | 281 | 0.78 | the share |
| a short post under a sidebar of thirty-three | 388 | 0.20 | the floor |
| a news article under its related links and comments | 1231 | 0.23 | the floor |
| the rest of the corpus articles | 501 to 1231 | 0.71 to 0.93 | both |

A floor alone would discard the announcement and the single-paragraph note, which an archive has as much reason to keep as anything else. A share alone would discard the same note as soon as its page grew a sidebar, which is most pages, and it would discard a long article whose page carries more comments than prose. The last two rows are also why the floor cannot simply be raised until it does the work by itself: an article's share falls as far as its furniture goes, and furniture has no bound.

**Where the floor comes from.** The library's own numbers bracket it without settling it: below 140 characters it stops counting a block as content at all, and at 500 it stops looking for more content in a page. What decides it is observation: the front pages seen measured 137 and about 250 characters, and 300 sits above both without reaching the length of the articles being kept.

**What that costs, exactly.** A page whose extraction is 299 characters or fewer survives only by being more than a quarter of its page. On a page busy enough, it does not survive at all, and that includes the 281-character note in this corpus: it is kept there because its page is plain, and the same note on a site with a sidebar is refused. The refusal is recorded rather than silent, and the response it was read from is stored whole, which is what makes a number this blunt tolerable while it is still the first guess.

**What the denominator counts, and what that lets a page do.** It is the page's text with the bodies of scripts, styles, `noscript` and templates left out, and everything else left in. That is not the same as what a reader would have seen: it counts navigation, banners and every block the scorer discards, and it counts text the page hides with CSS, which this never resolves. So a page can inflate its own denominator and push its own article under the share. What that buys is refusal of an extraction from the page doing it, on a response the archive stored whole either way, and the refusal is recorded rather than silent. It is a limit worth knowing, not a hole worth a partial defence: the same page could simply have served no article at all.

**These numbers were chosen against pages from few origins, which is not enough**, and they are expected to move for the same reason the ceilings below are. So every article records what it measured, and every refusal is written to the archive beside the capture it refused, as `<capture-id>.article-refused.json`. A count of refusals says the rule is firing; only the pages it refused and the shares the kept articles reached can say whether it fired on something it should have kept. That file is the queue a later pass reads.

Only pages the rule turned down are written there, never the many that the probe passed over. Those are most of the web, and a file for each of them would bury the few worth reviewing under the many that say nothing.

## Per-host rules, for the sites the scorer cannot read

A scoring heuristic reads markup that follows convention and fails on sites with a layout of their own. Everyone who has taken this seriously converged on the same architecture: the heuristic as the baseline, plus a per-host override expressed as data rather than as code. `ftr-site-config` has over a thousand files, one per domain; Mercury had roughly 150 extractors, one module per domain; Firefox reader mode has no override layer at all and is notoriously weak on exactly these sites.

The rules live in the archive, at `extraction-rules.json` beside `archeion.json`, and they are the one file there a person writes rather than the program. They are in the archive rather than in a configuration directory because a re-pass over these stored responses has to read them to produce the same articles again: an archive that travels without its rules extracts differently on the next machine and says nothing about why.

```json
{
  "hosts": {
    "lwn.net": {
      "why": "the subscription appeal is prose inside the article container",
      "strip": ["blockquote.ad", "table.IndexEntries"]
    },
    "tildes.net": {
      "why": "a topic page nests the replies inside the same element as the post",
      "body": ["div.topic-full-text"]
    }
  }
}
```

The key is the host as canonicalization spells it, which is the same string the archive files the item under: `www.` folded away, the case flattened. The match is exact, so a subdomain is a different site until one asks for something else. `why` is read by a person and never by the extractor, and it is there because a rule whose reason is not written down is a rule nobody can decide to delete a year from now.

### The two directives, and the pages that asked for them

**`strip` is where furniture survived the scorer.** A site that opens every article with an appeal for subscriptions, written as prose and placed inside the article container, defeats every generic instrument at once: it is several sentences of connected text, it carries no links a link-density rule would catch, and it sits exactly where the prose sits. The extraction carried it into the archive as the article's opening paragraph. The same page closed with a table of index terms that came through as a table of index terms. Neither is a defect in the scoring; both are cases where only somebody who has looked at the site can say which block is not the article.

**`body` is where the scorer chose the wrong subtree, or the page is not an article at all.** A forum's topic pages nest the replies inside the same element as the post, so what landed in the archive was a fifty-word post with five thousand words of other people's writing appended and nothing marking where the post ended. There is no container holding the post alone above it and no selector for the replies that the site would not eventually rename, so naming the post's own element is the only statement that separates them.

The second half of `body` is what makes it a statement about the site rather than about one page. The same forum's group listing carries the opening paragraph of every topic, which is several hundred words of genuine prose, so the sliver rule keeps it and a listing lands in the archive filed beside the articles. A host that has declared where its articles live has answered that too: **a page with none of it is not an article**. Falling back to the heuristic there would switch the rule off on exactly the pages that motivated it.

### What applying a rule changes

`body` runs first and `strip` second, which is the order the pipeline reads in: narrow to the article, then take the furniture out of it. It also makes a `strip` selector cheaper to write, since by the time one runs it only has to describe what is left.

For `body`, the first selector that matches anything wins, and what it matched becomes the whole document. A second selector is an alternative spelling of where the article is, not an addition: taking every selector's matches would glue two unrelated blocks together on the pages where a site's old and new markup both happen to appear. A match nested inside another match is dropped, since moving both would pull the inner one out of the outer one and hand back the article reordered.

The subtree is moved inside the tree it is already in. Nothing is serialized and parsed again, which matters because the ceilings above have already been paid by the time a rule runs, and a second parse would be a second chance to pay them.

Two guesses give way to a rule that named the article, and both are guesses this file argues for elsewhere. Both are **skipped** rather than left to be outrun:

- **The readability probe.** It weighs a document; a `body` rule leaves behind a document that is only the article; a short post is then refused for being what the rule cut it down to. The forum topic above is the shape that showed it, at 210 characters after narrowing.
- **The sliver rule.** Narrowing looks like it settles the comparison, since `page_chars` is counted after the rule and the article is now the page. It does not. `article_chars` is what the scorer kept and `page_chars` is what it was handed, and the scorer takes blocks out of the very container the rule named: a form, a link-dense table, anything `clean_conditionally` decides is not prose. What is left is then a sliver of a document the rule itself assembled, and the page the rule exists to rescue is refused. A short post above a five-button signup form reaches 59 against 399.

`share` is still recorded under a rule, and `rules` on the record is what keeps it usable: a share measured on a document a rule narrowed is not comparable with one measured on a whole page, and the calibration these numbers exist for has to be able to leave those rows out.

A `strip` says nothing about where the article is, so it changes neither, and a rule whose selectors all missed changes nothing at all: that page records `heuristic`. A host's rule is written for its articles, and the same host serves listings and index pages the rule never touches, so marking those as extracted under a rule would take the majority of a host's records out of the calibration just described.

A rule reaches only inside `<body>`. Everything else a selector can name is either meaningless, as an article that is the whole page, or destructive: a `strip` on `html` leaves the scorer no tree, and a `body` of `*` reparents the `<head>` into the document body, after which the page has no head and its title text starts counting as text the page said. A page with no body at all is a `<frameset>`, which has no article in it; a host that named where its articles are has therefore already answered for it, and a host that only named furniture has nothing there to take out.

### Arriving at a selector

A rule is written against pages that are already captured, which is what makes finding one cheap. The responses are on disk and a repass fetches no page, only the subresources a capture recorded as missed, so reading an article, changing the rule and repassing all run against bytes the site has already served.

The failure shows up in the article and its cause is only in the response. What survived into the Markdown is text, and a selector describes markup, so the text is where the search starts rather than what the rule can name. The response bodies are stored as blobs under their content hash, so a search across them finds the markup around that text, and the capture record carrying that hash says which page it came from.

Naming the container rather than the text is what makes a rule outlast the page it was written against. Text that survives is usually what a module renders in one of its states, and a selector aimed at that state stops matching the moment the module has something to show, which is when it contributes more rather than less. A footer listing a publication's other posts is the shape this keeps arriving as: empty it reaches the article as a two word paragraph, full it reaches the article as a block of links, and the element holding both is what the rule should name.

What a rule changed is then a comparison of two exports, since the question is not whether the furniture is gone but what else moved with it. An item that had an article and no longer has one has left the export entirely, which is what a `strip` naming too much costs: the sliver rule still runs on what the strip left behind, and a short page loses its article to it.

The `rules` field is the other half of that reading, and it lives on the records inside the archive rather than in the export. It says different things for the two directives. A page whose `strip` selectors all missed records `heuristic`, so counting those across a host separates a selector that describes the site from one that describes a single page. A `body` rule records the host on every page it reached, the pages where it found nothing included, because a page without the article a rule names is answered as not an article rather than handed back to the heuristic.

### A broken rule file costs extractions, never a capture

Everything that can go wrong degrades to the heuristic and reports why. A file that is absent is the ordinary case and says nothing at all. A file that no parser will read, one larger than 256 KiB, or a path that is not a regular file, leaves the extractor exactly where it was. A selector no CSS parser will read drops its host and no other: dropping the whole file would let one typo silently switch off every rule beside it, and dropping the selector alone would leave a rule doing something its author did not write.

The alternative would be a run that refuses to archive anything because a configuration file has a comma in the wrong place, and the response is the part that cannot be recovered later.

A selector arrives from a file this program did not write, so it is compiled once when the file is read rather than where it is used: the parser behind `Document::select` answers a bad selector with a panic, and only the explicit compile answers with an error.

### What is waiting, and what would make it move

`body` and `strip` are what real pages have asked for so far. The trigger for a third directive is the same as the trigger for these two: a real site captured, its Markdown coming out wrong, the page reduced by hand to the smallest markup that reproduces the failure, and the heuristic fixed if it can be. Only what survives that becomes a rule. The corpus does not discover these, it pins down what was already found, which is why every rule in it has to be shown failing without itself.

Two known gaps, neither built:

- **Bundled rules.** Everything is operator-written today. A corpus worth distributing is a licensing question before it is a code question, and there is no corpus yet.
- **A rule that a subdomain inherits.** Exact matching is what the sites seen needed. A newspaper whose jobs board is on a subdomain is the reason not to guess the other way.

## The document a site already published

Some sites serve a Markdown copy of every page beside its HTML, under the same path with a different extension. The convention is `llms.txt`, which proposes exactly that so a machine reading a site does not have to reconstruct its text, and it is spreading.

Capturing one such site produced twelve pages with no article, and eleven of them were `text/markdown`. Every refusal was correct under the rule above, since readability reads HTML and there was no markup to strip. What it left is an archive holding the best possible article text for those pages and filing it as an ordinary capture nobody would find as prose.

So a response that arrives as Markdown becomes the article. Nothing here can beat it: it is the author's own separation of the prose from the furniture rather than a guess at one.

**`text/markdown` and `text/x-markdown`, and nothing else.** `text/plain` is what many servers answer with for a `.md` path and it is deliberately not admitted, because it is also what they answer with for a changelog, a log and a `robots.txt`. A server that has not said the document is Markdown has not said it is prose.

### The record says nobody extracted anything

`rules` gains a third value, `"served"`, beside `"heuristic"` and `"site:<host>"`. The first two name which extraction found the prose; the third says no extraction happened at all. That distinction is the point of the field here: a reader comparing two articles has to be able to tell the one that was reconstructed from the one that was published.

The rest of the record follows from there:

- **`share` is present, with both counts equal.** It is not a placeholder. The document is the whole page, so the share is one by construction, and the sliver rule cannot fire on it. That is the same answer a `body` rule already gets and for the same reason: somebody who looked at the site said where the prose is, and a floor calibrated against a handful of origins does not overrule that. Keeping the pair present is also what preserves `share` being absent meaning one thing, which is the only reason that absence is worth reading.
- **`excerpt` and `byline` are absent.** The algorithm that fills them never ran, and a page description and an attribution are things this document did not say.
- **`cost` is measured, not skipped.** `document_bytes` is the decoded response, and `peak_open_elements` is what the generated markup measured on the way to the converter.

**`extractor_version` did not move, and that is a decision rather than an oversight.** No record that exists changed its meaning: `heuristic` and `site:<host>` say exactly what they said, and `served` is a value only records written afterwards can carry, which is the same ground on which the rules layer and the not-article marker did not bump it either. What did change is an absence, since a Markdown capture with no article beside it used to mean there was nothing to read. That is answered where absences are answered, by what a repass counts as a media type worth re-reading, and answering it there re-reads the handful of captures it applies to instead of rewriting every article record in the archive to carry a larger number.

### It is not trusted, and the converter is why

A served document arrives from the same place the HTML does. It passes through no converter that escapes anything, it can carry raw HTML, and the ceilings that bound a parse bound nothing in a document that is never parsed. Everything the HTML path learned about a page controlling its own document applies here with the safety net removed.

This file already answered the same question once, about the title: it was the one page-controlled string that did not arrive through the converter, and what was done with it was to **send it through the converter**. A served document is that problem with nothing left over, so it gets the same answer. The Markdown is read, turned into markup, and handed to the same `htmd` conversion every extracted article goes through.

What that buys is the property the rest of the archive already has. An `.article.md` is prose in a closed vocabulary, escaped by one escaper, whatever origin it came from. Storing the served bytes as they arrived would have made that a promise with two levels and left every future consumer to know which one it was holding.

Four things are handled during the read rather than left to the converter, because once markup exists there is no telling it apart from markup this program wrote:

- **Raw HTML becomes text.** It is the one construct a Markdown document can carry that an extracted article never can, since every renderer passes it through. It is kept as text rather than deleted, so the document still says what it said, inertly. A `<script>` served in a post is written `\<script>` and reads as the characters it is, which is what the title escaping already does.
- **A fenced block is left alone.** It is already inert to a renderer and it is often the point of the document, so escaping inside one would corrupt the code it holds and buy nothing.
- **A destination that exists to run loses its link.** The link text stays, under the same scheme policy the HTML converter applies.
- **An image's description is reduced to a description.** This is the round trip's own exception, and finding it is what made the shared policy above worth stating rather than assuming.

**An image's description may hold another image**, which is an example in the CommonMark specification rather than a corner case, and it is why a dropped image is tracked with a stack rather than a count. A count suppresses the inner end tag instead of the outer one, and the converter then keeps reading the description past where it ended: `![foo ![bar](/url) baz](…)` stores `foo ![bar baz](/url)`, and the word `baz` silently leaves the body of the article and becomes part of an attribute.

### The ceilings, on a document nothing parses

| ceiling | value | where it lands on this path |
|---|---|---|
| decoded document | 2 MiB | before anything is read, so it bounds how much work the read below can be asked to do |
| nesting depth | 256 | on the document's own structure, counted while it is read and before any markup exists |
| open elements | 2 048 | on the markup the read generates, which is where the record's measurement comes from |
| Markdown kept | 1 MiB | the file that gets written, exactly as on the HTML path |

**The depth ceiling is the one that decides whether the process survives the document**, and it is the same number the HTML path applies to its tree. A document of nothing but `>` opens a blockquote per level; the converter walks its tree with a stack frame per level; two thousand levels is a four kilobyte response that ends the whole run with a stack overflow rather than being refused as one page. Counting depth on the generated markup instead would mean generating it first, which is paying the cost the guard exists to avoid, so it is counted on the document's own structure as it is read, and the read stops at the first level past the ceiling.

A level of Markdown counts as one nested container, so a list level counts twice, once for the list and once for the item. That errs toward refusing, which is the direction a guard against a stack overflow has to err in.

**The open-element count is a measurement, not the bound.** A level of Markdown opens at most two elements, so under a depth ceiling of 256 it cannot reach 2 048. It is taken because it is what fills `peak_open_elements` on the record, and because the sentence before this one is an assumption about how a document's nesting maps onto the markup it generates: if it ever fires, that mapping is what changed. The quadratic parse it was originally measured against cannot arise here at all, since the generated markup is balanced, and unbalanced raw markup in the document leaves the read as escaped text and so opens nothing.

Measured on documents built to be the worst of each shape, at or just over the ceiling each one meets:

| document | wall clock | outcome |
|---|---|---|
| 2 MiB of list items | 0.77 s | article |
| 2 MiB of one-character paragraphs | 0.69 s | article |
| 2 MiB of emphasis spans | 0.53 s | article |
| 2 MiB of inline links | 0.42 s | article |
| 2 MiB of `<` | 0.74 s | article |
| a blockquote nested 500 deep | 0.02 ms | refused, over the depth ceiling |
| a blockquote nested 2 000 deep | 0.10 ms | refused, over the depth ceiling |
| a list nested 2 000 deep | 0.10 ms | refused, over the byte ceiling |
| a table of 200 000 rows | 0.05 ms | refused, over the byte ceiling |

Every shape that nests is turned away in well under a millisecond, because the guard reads the document instead of building anything from it. Nesting a list costs bytes quadratic in its own depth, since every level pays for its own indentation, so the byte ceiling reaches those first; nesting a blockquote is linear in depth, so the depth ceiling is what reaches it.

The refusals are spelled by the same type the HTML path refuses with, so a page turned away for cost reads the same in a run report whichever of the two it came through.

### Which CommonMark extensions are read

Two, and each is there because of what the document looks like without it rather than because of what it adds.

- **Tables**, because the converter writes a table back out as a table, while a pipe table left unparsed collapses into one mangled paragraph and stops being a table at all.
- **YAML metadata blocks**, because a document opening with `---` and no extension to read it parses as a horizontal rule followed by a setext heading, so its front matter becomes a heading the document never had.

Strikethrough is the shape of the ones left out. The converter has no Markdown for `<del>`, so reading `~~gone~~` loses the marks, where leaving it unread keeps the characters standing as the text they already were. An extension is worth enabling only when parsing it preserves more than not parsing it does, and the same test settles the next one that is proposed.

### A page and its Markdown are two items, deliberately

`example.com/posts/a.html` and `example.com/posts/a.md` are different URLs, so they are different items with their own captures, and this change leaves them that way.

Inferring the relation from a shared path stem is a guess about a server's routing, and a wrong one silently merges two pages that were never the same. A page that **declares** the relation, through a `rel="alternate"` link or a `Link` header, is a different and better signal, and nothing captured so far has carried one. When something does, that is the moment to build it.

**Leaving them unrelated is also what satisfies the constraint this whole feature had to satisfy.** A site that serves Markdown badly, truncated, stale or generated wrong, must not silently beat a correct extraction from its own HTML. Because nothing is related, nothing is replaced: the served document produces its own article beside its own capture, the HTML page keeps its own extraction, and no preference is expressed anywhere. Relating the two is precisely what would create the need for a preference rule, and that rule is the decision deliberately deferred alongside.

## What a hostile page costs

The archive fetches addresses it was pointed at by other pages. Two of the libraries it hands them to have costs that grow faster than the input, so a page a few hundred kilobytes long can be built to cost minutes, and the cost ceilings here exist because a measurement said so. The excerpt ceiling is different: it protects the size of a review clue whose purpose is to identify the refused prose, not to store the page's whole description.

| ceiling | value | what it bounds | where it lands |
|---|---|---|---|
| decoded document | 2 MiB | how much work any of the below can be asked to do | the run report |
| open elements | 2 048 | the parse | the run report |
| nesting depth | 256 | the scoring pass | the run report |
| elements scored | 50 000 | the scoring pass on a wide document | the run report |
| excerpt kept | 4 KiB | the review clue copied from page-controlled metadata | the stored record |
| Markdown kept | 1 MiB | the file that gets written | the stored record |

Only the last two are recorded in the archive, as `truncated`, because they are the ones where a derived record still exists to describe what was cut. The first four produce no article or refusal record at all, so what they leave is an entry in the run's report naming the URL and the ceiling. That is the honest state of it: a page refused for cost and a page that simply was not an article look the same on disk.

### The decoded document ceiling, and what news pages measured

The decoded document ceiling started at 1 MiB because hostile markup already had a measured cost below that size. The first real article to push through it measured 1 428 771 decoded bytes and produced no article, while a sample of ninety-six article pages from large news sites put ordinary portal articles close to the same boundary. The largest pages in that sample were not large because the article was large: the article text was usually less than one percent of the decoded document.

| page shape | decoded bytes | page text chars | article text chars | article share of bytes |
|---|---:|---:|---:|---:|
| refused news article | 1 428 771 | not recorded | not recorded | not recorded |
| large AP article | 989 238 | 38 519 | 7 032 | 0.71% |
| large AP article | 964 629 | 39 659 | 8 098 | 0.84% |
| large AP article | 939 582 | 35 759 | 4 378 | 0.47% |
| large AP article | 935 652 | 38 810 | 7 327 | 0.78% |
| large CNBC article | 889 710 | 4 576 | 2 468 | 0.28% |
| large AP article | 888 162 | 38 263 | 6 824 | 0.77% |
| large CBS live article | 863 510 | 21 639 | 19 198 | 2.22% |
| BBC article | 406 203 | 8 968 | 4 939 | 1.22% |
| Guardian article | 318 106 | 7 120 | 5 174 | 1.63% |
| NPR article | 163 000 | 8 991 | 7 666 | 4.70% |

That leaves three possible responses. A per-host rule cannot help at this point in the pipeline, because rules need a tree and the document has to pass the ceilings before a tree exists. A cheaper guard is already present for the shape that makes parsing expensive, the open-element scan. Weighing some other pre-tree value would need a new scanner with its own definition of visible text, and no measurement yet says that extra moving part is the thing needed. The smaller change is to move the outer decoded-document ceiling to 2 MiB, keep the open-element and depth ceilings where the hostile-cost measurements put them, and let `AdmissionCost.document_bytes` continue collecting the distribution that can later justify moving the ceiling again.

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
| 1 MiB | about 72 s, extrapolated |
| 2 MiB, the decoded-document ceiling | about 288 s, extrapolated |

Doubling the input quadruples the time. This is the reason the byte ceiling is still far below the 8 MiB it started at, and the reason the byte ceiling is not enough on its own.

Neither of the other guards helps. The depth ceiling is measured on a tree, and by the time there is a tree the cost has been paid; the element ceiling is applied by the scorer, one step later still. So the count is taken on the bytes, before anything is parsed, in `markup_scan`. A raw scan reads 8 MB in 3 ms where the token-stream parser takes 4 s on the same input, and it stops at the first element past the ceiling.

Reading bytes rather than parsing them means the scan has to know a few things or it will refuse ordinary pages: void elements never close and would otherwise accumulate, so a gallery of images would look like unbalanced markup; `<` is an operator in every language a page embeds, so script and style bodies are skipped; attribute values hold `<` and `>` in ordinary prose; and a comment ends at `-->` and not at the first `>` inside it. Where it is still wrong it is wrong upward, as with prose that was never escaped, so it refuses rather than admits.

### The ceilings are expected to move

They are set where a hostile page is certainly refused, not where a real page is certainly kept, and the distance between those two is unknown. So every article records what it actually measured, in `cost`, for every page and not only for the refused ones: a count of refusals says whether a ceiling is firing, and only the values real articles reach can say whether a ceiling would start refusing them. After enough real captures, the ceilings move against that distribution rather than against a guess.

## The record

```json
{
  "markdown_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "extractor_version": 5,
  "rules": "heuristic",
  "word_count": 1240,
  "share": {
    "article_chars": 6120,
    "page_chars": 7043
  },
  "excerpt": "Bread is mostly patience.",
  "byline": "J. Writer",
  "accessible_for_free": null,
  "cost": {
    "document_bytes": 48213,
    "peak_open_elements": 42
  }
}
```

`share` is what the sliver rule measured: the article's own text, and the text of the whole page it came out of. It is recorded for the pages that were kept and not only for the ones that were refused, on the same terms as `cost`: a file per refusal says the rule is firing, and only the shares real articles reach can say whether it is about to start firing on them. The two counts and not the ratio between them, because a ratio is a division somebody already did at a precision they chose, and the question these exist to answer is about the distribution of both sides.

It is absent on records written before the rule existed, which is not the same as a page whose text measured nothing. A reader that treated a missing `share` as a pair of zeroes would fill the calibration it exists for with pages nobody measured.

### What the page declared about paying for it

A post behind a paywall is prose like any other, and every instrument above reads it as a healthy article: the word count is ordinary, and the sliver rule sees a share close to one, because what the page served really is almost all teaser and pitch. One capture measured a paywalled post at 303 words and a share of 1286 of 1584 characters, both numbers a short, complete post could just as easily have produced. Nothing about the shape of the text tells the two apart.

Something else does. schema.org defines `isAccessibleForFree` on `CreativeWork`, and a paywalled page's own JSON-LD carries it as `false`. That is a declaration the response made about itself, not a guess this extractor formed by reading the prose, and it is the only field on this record that can be. It is read out of the JSON-LD blocks metadata extraction already parsed and stored, so no page is read twice to fill it.

`accessible_for_free` is `Option<bool>`, and the three values it can hold say three different things: `true` is the page declaring itself whole, `false` is the page declaring a wall, and `null` is nothing declared, which is what an ordinary page and an old record both produce. `null` must not be read as `true`: no page said this article is complete, only that none of them said otherwise. A bare boolean cannot make that distinction, which is why this is not one.

The JSON-LD a captured page carries is hostile input, the same as the markup around it. Only a literal JSON boolean under the key is read as a declaration; a string, a number, `null` under the key, or any other shape is silently not one, on the same terms a malformed `author` elsewhere in metadata resolution is read as absent rather than guessed at. The blocks are flattened the way metadata resolution already flattens them, a bare object, a list of them, or a `@graph` holding the list, and the two compose, since a list whose entries hold graphs is a shape real pages ship.

The two answers are not reached on the same terms, and the asymmetry is the point. A page describes several works in one block, its publisher and the site around it among them, and a site being free says nothing about the post on it, so a claim of free access counts only from a node typed as the document being read. Taking it from anywhere would let a page make the archive assert that a truncated note is whole, which is worse than the archive saying nothing at all. A refusal is not held to that test and counts from any node, because a page has no reason to declare a wall it does not have. For the same reason, nodes that disagree resolve toward the paywall: any node saying `false` decides it, since missing a real wall defeats the reason this field exists and a spurious one costs a reader nothing worse than a second look at a note that turns out whole.

This is deliberately not a text heuristic and deliberately not aimed at one publisher. Matching on a page's own `paywall` class names, or hardcoding anything about the one platform the capture that motivated this came from, would have been a guess about markup, and this project already has one rule about that: the signal is what the response declared, never a guess from the text.

**A page behind a paywall is still archived as an article.** It is prose, it is what the server sent, and refusing it would lose the teaser and every other field on this record for no gain. It is kept and marked, not dropped, and the marking is carried into the export so a vault reader can tell without opening the archive.

`accessible_for_free` did not move `extractor_version`. It is a new, additive field, and its absence on a record written before it existed is already the correct reading: nothing here said either way, which is exactly what an unmarked archive item should say. See the `extractor_version` history below.

The absence is answered where absences are answered, by what a repass counts as worth re-reading, which is the same place the served-Markdown absence was answered and for the same reason. An article at the current version whose record carries no declaration, over a page whose stored JSON-LD does carry one, is stale for that alone. That reaches the captures this field exists for, the ones taken before anything read the declaration, without moving a version that would rewrite every article in the archive to reach a handful of them.

An article is stale for a second reason of the same kind: the metadata record beside it is. What that record said is inside the article, since the page's title is what the article's first heading is built from, so an article derived from a reading that is about to be replaced was derived from something this pass has already decided was wrong. Judged only by its own version it says it is current, and saying so is what would keep every later pass from repairing it: the note would then carry one spelling in its front matter and the other in its heading, permanently. Stating the dependency is cheaper and more exact than moving a version to stand in for it, which is the argument the paragraph above makes for a declaration nobody read.

A refused page gets a record of its own instead of this pair, holding the same measurement, the excerpt and nothing else:

```json
{
  "extractor_version": 5,
  "rules": "heuristic",
  "share": {
    "article_chars": 137,
    "page_chars": 1188
  },
  "excerpt": "Written by hand, published from a laptop on a kitchen table."
}
```

There is no field naming the rule that refused it. One rule refuses here and its inputs are the two counts, so the comparison is readable from the record itself; a second rule is what would make naming them worth a field, and it will arrive with its own version bump.

The excerpt is the page-controlled field that both article and refusal records carry, and the reader of these records refuses a file over 64 KiB. A page serving an enormous description could therefore write a refusal that would not read back, which would be reported by path rather than silently skipped. Both article and refusal records cut the excerpt to 4 KiB before writing. When that happens, `truncated` carries `excerpt`, and the stored value is only a prefix kept for review. The excerpt below is shortened for display; a real record with this flag stores a prefix close to the ceiling:

```json
{
  "extractor_version": 5,
  "rules": "heuristic",
  "share": {
    "article_chars": 137,
    "page_chars": 1188
  },
  "excerpt": "Written by hand, published from a laptop on a kitchen table.",
  "truncated": ["excerpt"]
}
```

The byline is also page-controlled, but it cannot answer the same way. A cut excerpt is still an excerpt: the record says it is a prefix, and the reader knows what kind of value it holds. A cut byline can become a false attribution. If a page named twenty authors and only the first three fit, storing those three would claim an authorship the page did not claim. So an article byline over 4 KiB is not stored at all. When that happens, `byline` is absent and `truncated` carries `byline`, which says the page did provide an attribution but the record could not carry it honestly:

```json
{
  "markdown_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "extractor_version": 5,
  "rules": "heuristic",
  "word_count": 1240,
  "share": {
    "article_chars": 6120,
    "page_chars": 7043
  },
  "excerpt": "Bread is mostly patience.",
  "byline": null,
  "accessible_for_free": null,
  "truncated": ["byline"],
  "cost": {
    "document_bytes": 48213,
    "peak_open_elements": 42
  }
}
```

The 4 KiB ceiling is the first guess, not a measured distribution. The local corpus had too few bylines to calibrate a real percentile, so it borrows the already documented excerpt budget as a conservative record budget: enough for verbose real attributions, small enough that one free-text field cannot push the JSON record toward the 64 KiB reader ceiling. Like the other ceilings, it is meant to move once `AdmissionCost` and real archives give better evidence.

Article-state records written before these ceilings can already be larger than the reader accepts. A derived article, refusal or not-article record over the 64 KiB record ceiling is treated as absent rather than fatal: the response body is still the authority, and `repass` can rebuild the derived answer from it. The `.article.md` beside an oversized article record may still be on disk, but the pair is not trusted until the record is readable again.

A page the extractor read and judged not to be an article gets the smaller marker below:

```json
{
  "extractor_version": 5,
  "rules": "heuristic"
}
```

The same `rules` rule applies here as on an article or refusal. It is `heuristic` when the scorer made the decision, `site:<host>` when a host rule actually reached the page or a `body` rule answered that the article it names is not present on this page, and `served` when the response was Markdown and held no prose, which is a document read and declined exactly as an HTML listing is.

`rules` names how the prose in this record was obtained: `heuristic` when nothing was said about the host or when what was said matched nothing on this page, `site:<host>` when a rule actually reached it, and `served` when nothing scored anything because the response already was the prose. It stays one string across all three, because every reader that filters on it compares it to a string and turning it into an object would break all of them for nothing. A value this extractor cannot account for is refused rather than read as `heuristic`, which would claim a page was read with nothing said about it, and that refusal is what keeps a reader written before `served` existed from misreading one of those records rather than turning one away.

A record for a document the site published looks like this. `excerpt` and `byline` are `null` because nothing derived them, `accessible_for_free` is `null` because a Markdown document has no JSON-LD for anything to read, and its two counts are equal because the document is the whole page:

```json
{
  "markdown_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "extractor_version": 5,
  "rules": "served",
  "word_count": 1240,
  "share": {
    "article_chars": 6120,
    "page_chars": 6120
  },
  "excerpt": null,
  "byline": null,
  "accessible_for_free": null,
  "cost": {
    "document_bytes": 7104,
    "peak_open_elements": 6
  }
}
```

`byline` is what the algorithm found in the page's own markup and is not the resolved author in the metadata record: the two disagree often, and collapsing them would hide which one to look at when an attribution comes out wrong. `word_count` counts the prose and not the heading, which is a title the metadata record already holds; it stays a rough figure for sorting and filtering, which is why it is not what the sliver rule weighs. `truncated` is absent when nothing was cut, which is the ordinary case. On an article record, `markdown` means the prose file beside the record is a prefix of the article, `excerpt` means the excerpt field itself is a prefix, and `byline` means the byline was too large to store as a truthful attribution. On a refusal record, only `excerpt` can appear there.

`markdown_sha256` is the address of the document beside it, and it is what makes the pair safe to rewrite. Ordering alone is enough only the first time: writing over an existing pair and stopping between the two files leaves new prose beside an old record, both present, both parsing, and every field describing something that is no longer there. A reader that finds the two disagreeing reports no article, because the response the article was derived from is still in the archive and the pass that re-extracts will simply redo it.

`extractor_version` is bumped when the meaning of a field or a rule that fills one changes, not when a field is added, on the same terms as the metadata record. It is 5 today. It became 2 for the sliver rule: `share` arriving beside the counts would not have been enough on its own, but a page can now produce prose and still not be stored as an article, so the absence of a record beside a capture stopped meaning what it meant at 1. It became 3 when HTML article links and image descriptions stopped reaching the stored Markdown as page-controlled syntax. It became 4 when which address an image carries and whether an anchor around one is a link at all changed: a record written under 3 names a different address for the same image and spells a linked picture as characters that are not a link, so an export built from it differs from one built from a rebuild. It became 5 when an `iframe` whose `src` resolves to an address stopped vanishing from the article: a record written under 4 simply lost the element, so its absence of a link is not evidence there was nothing there to link.

The rules layer, the not-article marker, the byline bound and `accessible_for_free` did not bump it. For the byline, a present value still means the attribution the page carried, and `truncated: ["byline"]` is an added answer only records written after the bound can carry. The records that exist did not change their meanings: `article_chars` and `page_chars` mean exactly what they meant at 2, and `rules` says whether a rule reached the page. The marker is a new record type with the same version as the extractor that made the decision. An older absence stays stale to a repass because it is absence, not because every article record needs a new number. `accessible_for_free` is the same shape again: a record written before it existed said nothing about a paywall, and reading its absence as "nothing declared" is not a claim that record never had a chance to make, it is the true answer either way.

## What was deliberately left out

- **Metadata for a document a site published.** The metadata extractor reads tags and a Markdown document has none, so a served capture has no title, author or date, and it appears in `list` and in `export` without one. Reading YAML front matter or the document's first heading would answer it, and both are changes to metadata extraction with precedence rules of their own to settle against the ones already written down there.
- **Relating a page to the Markdown beside it.** Covered above.
- **A third directive.** `body` and `strip` are what real pages have asked for. The fivefilters corpus grew to twenty directives against real sites, and every one of them arrived because a site demanded it; adding one here follows the same rule rather than anticipating it. What is already known to be missing, and deliberately waiting, is in the rules section above.
- **Plain text beside the Markdown.** Covered above.
- **Images pulled into the prose.** An article's images are already captured as subresources and addressed by content hash. Rewriting the Markdown to point at them is a question about how a reader resolves references, which belongs to the reader.
- **Pagination.** An article split across numbered pages is captured as the several pages it is served as. Stitching them is a per-site rule in every implementation that does it, so it waits for the rules layer.
- **Language-aware word counting.** `word_count` splits on whitespace, which is wrong for languages that do not use it. It stays a rough figure for sorting and filtering rather than a measurement, and nothing decides anything on it: the sliver rule counts characters precisely so that a page in such a language is judged the same way as any other.
- **A ceiling on wall clock.** Every guard here bounds a shape that was measured. None of them bounds a shape that was not, and a per-document time limit is the only thing that would. It is not built because a thread cannot be stopped from outside in this language, so such a limit would bound how long a capture waits without bounding what it spends, and a host serving many such pages would saturate the machine either way.
- **A guard on the metadata path.** The quadratic parse described above is a property of reading hostile markup, not of this module: metadata extraction takes 0.6 s on the same input where this took 18 s. It is bounded there, but by a memory ceiling that happens to cut the blowup short rather than by anything aimed at it. Giving it the same scan is its own change.

## Testing

Extraction quality varies by site and cannot be asserted exactly. Pinning the current output as expected would freeze today's behavior as correctness and break on every improvement, so the corpus asserts **bounds** instead: prose that must survive, furniture that must not, the heading hierarchy, and a range for the word count.

The fixtures are hand written, one file of markup and one of expectations, and they are minimal reproductions of shapes rather than saved pages. Real pages would carry a licence question into a public repository and hundreds of kilobytes per case.

This means the corpus does not discover the sites that need work; it cannot, since it only contains shapes someone already thought of. Discovery happens by running the tool. When a real page comes out wrong it is reduced by hand to the smallest markup that reproduces the failure, and that becomes a fixture. The corpus pins down what was found, so it cannot come back.

The sliver rule above is the first thing that arrived that way, and the front page it was written for is in the corpus beside the listing that was imagined. Both stay: the imagined one covers the easy shape, and the easy shape is still worth pinning. So is the pairing, since a case that only proved the rule refuses something would be satisfied by a rule that refuses everything short. The genuinely short post beside it is what makes the refusal mean anything.

A fixture may declare the rule its host has been told, and two of them do. Those cases assert twice: that the extraction with the rule is the one declared, and that the extraction without it is not. The second is what keeps a rule from becoming decoration, because a rule that stopped being needed, whether the scorer improved or it never was needed, would otherwise sit in the corpus reading as evidence that a site has to be told while proving nothing.

The corpus is markup, so the served path is not in it and could not be: there is nothing to score and no bound to assert about the scoring. What covers it instead is a case per decision beside the code, plus one test that crawls a Markdown post through a link on an ordinary index, against a server it starts on loopback. That last one is not redundant with the others. Whether the crawl engine follows a link to a document that is not markup and hands the response over as a page at all is a property of the engine and its configuration, it compiles either way, and every test that builds its own page events would pass either way.

One shape found in the wild is deliberately not in the corpus. A forum topic whose replies outweigh the post by fifty to one comes back as the whole thread, and the reduced version of it does not: whether the scorer picks the post or the element above it turns on the arithmetic of how much text sits where, and a hand-written page small enough to read is too small to tip it. Contorting the markup until it tipped would pin an accident of the scoring rather than a property of the site, so the case lives here as a measurement instead.
