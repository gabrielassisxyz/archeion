//! Reading the candidates a `srcset` lists.
//!
//! Two parts of this project read the same attribute for different reasons, and both are wrong
//! in the same expensive way if they split it naively, so the grammar lives here once. Metadata
//! extraction records every address the page listed; the Markdown conversion picks the one
//! rendition worth showing a reader. Only the second needs the descriptor, and it is the reason
//! this yields candidates rather than addresses.

/// One entry of a `srcset`: an address and the descriptor that followed it, if any.
pub(crate) struct Candidate<'a> {
    pub(crate) url: &'a str,
    /// What the page wrote after the address, `1456w` or `2x`, empty when it wrote nothing.
    /// The specification reads an absent descriptor as `1x`, so absent and zero are not the
    /// same thing and this keeps them apart.
    pub(crate) descriptor: &'a str,
}

/// The candidates a `srcset` lists, in the order it lists them.
///
/// The separator is a comma, and a comma is legal inside a URL: an image served through a
/// transformation network spells its parameters that way, `w_320,h_213,c_fill`. Splitting on
/// the character alone turns one candidate into a handful of fragments, and a fragment is a
/// relative reference, so the archive ends up asking the page's own origin for addresses that
/// were never on the page. That is a request the page did not make, which is a rate limit at
/// best and a page choosing where the archive knocks at worst.
///
/// What separates candidates is therefore the end of the URL rather than the character: a URL
/// runs to whitespace, and the comma that follows it, or the commas it ends with when it
/// carries no descriptor, are the separator. This is the specification's own reading, and the
/// only one under which `a.png,b.png` is the single address a browser requests.
///
/// What is deliberately not done is validating the descriptor. A browser drops a candidate
/// whose descriptor is malformed, and this keeps it, because the two are answering different
/// questions: a browser is choosing which one image to fetch for a viewport, and the archive is
/// recording every address the page listed. It buys no safety either way, since a page that
/// wants a request made writes a descriptor that is valid.
///
/// Lazy rather than collected, so an attribute holding more candidates than a caller will keep
/// costs the caller's own ceiling rather than a vector the size of the attribute.
pub(crate) fn candidates(srcset: &str) -> impl Iterator<Item = Candidate<'_>> {
    let bytes = srcset.as_bytes();
    let mut position = 0;

    std::iter::from_fn(move || {
        loop {
            // Whitespace and commas are skipped together, so an empty candidate between two
            // separators disappears rather than becoming an empty address.
            while position < bytes.len()
                && (bytes[position].is_ascii_whitespace() || bytes[position] == b',')
            {
                position += 1;
            }
            let start = position;
            while position < bytes.len() && !bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            if start == position {
                return None;
            }

            let token = &srcset[start..position];
            let url = token.trim_end_matches(',');
            let mut descriptor = "";
            // A URL that kept its last character ran to whitespace rather than to a separator,
            // so a descriptor follows and reaches to the next comma. A descriptor may hold one
            // inside parentheses, and being inside them is a state rather than a depth: a
            // second opening parenthesis is content there, so counting it would hide the comma
            // that ends the candidate and swallow the next one.
            if url.len() == token.len() {
                let descriptor_start = position;
                let mut inside_parentheses = false;
                let mut descriptor_end = bytes.len();
                while position < bytes.len() {
                    match bytes[position] {
                        b'(' => inside_parentheses = true,
                        b')' => inside_parentheses = false,
                        b',' if !inside_parentheses => {
                            descriptor_end = position;
                            position += 1;
                            break;
                        }
                        _ => {}
                    }
                    position += 1;
                }
                descriptor = srcset[descriptor_start..descriptor_end.min(srcset.len())].trim();
            }
            if !url.is_empty() {
                return Some(Candidate { url, descriptor });
            }
        }
    })
}

/// The address of the candidate offering the most, or nothing when the attribute lists none.
///
/// Most is by width first, since a width descriptor is the page saying how many pixels the file
/// actually holds, and by pixel density only when no candidate carries a width. A candidate with
/// no descriptor is `1x` by the specification rather than a width of zero, which is the reading
/// that keeps a bare single-candidate `srcset` usable instead of ranking it below everything.
///
/// Ties keep the earlier candidate, so a page listing the same size twice is read the way it was
/// written and an attribute read twice answers the same both times.
pub(crate) fn widest(srcset: &str) -> Option<&str> {
    let mut best: Option<(Rank, &str)> = None;
    for candidate in candidates(srcset) {
        let rank = rank_of(candidate.descriptor);
        if best.as_ref().is_none_or(|(current, _)| rank > *current) {
            best = Some((rank, candidate.url));
        }
    }
    best.map(|(_, url)| url)
}

/// How much a descriptor claims its candidate offers.
///
/// Width outranks density outright rather than being converted into it: the two measure
/// different things and a page that mixes them is already outside what the specification allows,
/// so there is no exchange rate to invent.
#[derive(PartialEq, PartialOrd)]
enum Rank {
    Density(f64),
    Width(u64),
}

fn rank_of(descriptor: &str) -> Rank {
    let descriptor = descriptor.trim();
    if let Some(width) = descriptor
        .strip_suffix(['w', 'W'])
        .and_then(|width| width.trim().parse::<u64>().ok())
    {
        return Rank::Width(width);
    }
    if let Some(density) = descriptor
        .strip_suffix(['x', 'X'])
        .and_then(|density| density.trim().parse::<f64>().ok())
        .filter(|density| density.is_finite())
    {
        return Rank::Density(density);
    }
    // An absent descriptor is `1x`, and so is one nothing here could read: a candidate a page
    // wrote badly is still a candidate, and treating it as the default is what a browser that
    // could not use the descriptor would be left with.
    Rank::Density(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(srcset: &str) -> Vec<&str> {
        candidates(srcset).map(|candidate| candidate.url).collect()
    }

    #[test]
    fn a_candidate_keeps_the_descriptor_that_followed_it() {
        let read: Vec<(&str, &str)> = candidates("/a.png 424w, /b.png 1456w")
            .map(|candidate| (candidate.url, candidate.descriptor))
            .collect();
        assert_eq!(read, [("/a.png", "424w"), ("/b.png", "1456w")]);
    }

    /// The comma inside a transformation network's parameters is not a separator, which is the
    /// defect this grammar exists for: split on the character and the archive asks the page's
    /// own origin for fragments that were never addresses.
    #[test]
    fn a_url_holding_commas_is_one_candidate() {
        assert_eq!(
            urls("https://cdn.example/fetch/w_320,h_213,c_fill/one.jpeg 320w"),
            ["https://cdn.example/fetch/w_320,h_213,c_fill/one.jpeg"]
        );
    }

    /// A candidate ends at whitespace and not at the comma, so `a.png,b.png` is the single
    /// address a browser requests and the descriptor of it is empty. Reading it as two is the
    /// mistake the whole grammar is written to avoid.
    #[test]
    fn a_candidate_with_no_descriptor_reports_an_empty_one() {
        let read: Vec<(&str, &str)> = candidates("/a.png,/b.png")
            .map(|candidate| (candidate.url, candidate.descriptor))
            .collect();
        assert_eq!(read, [("/a.png,/b.png", "")]);

        let separated: Vec<(&str, &str)> = candidates("/a.png, /b.png")
            .map(|candidate| (candidate.url, candidate.descriptor))
            .collect();
        assert_eq!(separated, [("/a.png", ""), ("/b.png", "")]);
    }

    #[test]
    fn the_widest_candidate_is_the_one_with_the_largest_width() {
        assert_eq!(
            widest("/small.png 424w, /large.png 1456w, /middle.png 848w"),
            Some("/large.png")
        );
    }

    /// The case the whole rule exists for: a page listing the same picture at four widths, where
    /// the address a reader should be given is the last one anybody would reach by taking the
    /// first candidate.
    #[test]
    fn the_widest_wins_wherever_the_page_put_it() {
        assert_eq!(widest("/a.png 1456w, /b.png 424w"), Some("/a.png"));
        assert_eq!(widest("/a.png 424w, /b.png 1456w"), Some("/b.png"));
    }

    #[test]
    fn density_decides_when_no_candidate_carries_a_width() {
        assert_eq!(
            widest("/one.png, /two.png 2x, /three.png 1.5x"),
            Some("/two.png")
        );
    }

    /// A bare candidate is `1x` and not a width of zero, so a `srcset` naming one address is
    /// usable rather than ranked below everything.
    #[test]
    fn a_single_candidate_with_no_descriptor_is_still_the_widest() {
        assert_eq!(widest("/only.png"), Some("/only.png"));
    }

    /// A width beats any density, since the two measure different things and a page that mixes
    /// them has already left what the specification allows.
    #[test]
    fn a_width_outranks_a_density() {
        assert_eq!(widest("/dense.png 4x, /wide.png 100w"), Some("/wide.png"));
    }

    #[test]
    fn a_descriptor_nothing_can_read_is_treated_as_the_default_density() {
        assert_eq!(widest("/broken.png ???, /plain.png 2x"), Some("/plain.png"));
    }

    #[test]
    fn an_empty_srcset_offers_no_candidate() {
        assert_eq!(widest(""), None);
        assert_eq!(widest("  ,  , "), None);
    }
}
