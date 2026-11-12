//! Splitting an input into model-sized windows.
//!
//! GLiNER v2.1 is trained at a fixed sequence length (`max_len`, 384
//! subtokens for the small checkpoint) and the label prompt is charged
//! against that same budget. A memory long enough to exceed what is
//! left cannot be sent to the backbone in one pass, so it is covered by
//! a series of overlapping windows and the spans are merged afterwards.
//!
//! Windows overlap by the maximum span width the model can emit. An
//! entity is only recognisable when every one of its words sits in the
//! same window, so an overlap narrower than the widest span would let
//! entities fall between two windows and go unreported.

/// One window over the word list, as a half-open range of word indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Index of the first word in the window.
    pub start: usize,
    /// Index one past the last word in the window.
    pub end: usize,
}

/// A word the planner could not place: on its own it is longer than a
/// whole window, so no window can contain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OversizedWord {
    /// Index of the word in the input word list.
    pub index: usize,
    /// Subtoken length of the word.
    pub subtokens: usize,
}

/// The outcome of planning windows over one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Windows to run, in input order. Empty when there are no words.
    pub windows: Vec<Window>,
    /// Words skipped because a single word exceeded the whole budget.
    /// Reported so the caller can log what the model never saw.
    pub oversized: Vec<OversizedWord>,
}

/// Lay `subtoken_lens` (one entry per word) out into windows of at most
/// `budget` subtokens, overlapping by `overlap` words.
///
/// `budget` is the room left for words once the label prompt and the
/// structural tokens have been charged, and must be at least 1.
pub fn plan(subtoken_lens: &[usize], budget: usize, overlap: usize) -> Plan {
    debug_assert!(budget >= 1, "caller rejects a budget that fits no words");
    let mut windows = Vec::new();
    let mut oversized = Vec::new();

    let mut start = 0usize;
    while start < subtoken_lens.len() {
        // A word longer than an entire window can never be placed. Drop
        // it and resume: keeping it would stall the walk forever.
        if subtoken_lens[start] > budget {
            oversized.push(OversizedWord {
                index: start,
                subtokens: subtoken_lens[start],
            });
            start += 1;
            continue;
        }

        let mut end = start;
        let mut used = 0usize;
        while end < subtoken_lens.len() && used + subtoken_lens[end] <= budget {
            used += subtoken_lens[end];
            end += 1;
        }
        windows.push(Window { start, end });

        if end >= subtoken_lens.len() {
            break;
        }

        // Step back so an entity straddling the cut is whole in the next
        // window, while always advancing by at least one word.
        let step_back = overlap.min(end.saturating_sub(start).saturating_sub(1));
        start = end - step_back;
    }

    Plan { windows, oversized }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_input_needs_one_window() {
        let p = plan(&[1, 1, 1], 10, 2);
        assert_eq!(p.windows, vec![Window { start: 0, end: 3 }]);
        assert!(p.oversized.is_empty());
    }

    #[test]
    fn windows_cover_every_word() {
        let lens = vec![1usize; 20];
        let p = plan(&lens, 5, 2);
        let mut covered = vec![false; lens.len()];
        for w in &p.windows {
            for c in covered.iter_mut().take(w.end).skip(w.start) {
                *c = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "every word lands in a window");
    }

    #[test]
    fn no_window_exceeds_the_budget() {
        let lens = vec![3usize; 30];
        let budget = 10;
        let p = plan(&lens, budget, 4);
        for w in &p.windows {
            let used: usize = lens[w.start..w.end].iter().sum();
            assert!(used <= budget, "window {w:?} used {used} > {budget}");
        }
    }

    #[test]
    fn consecutive_windows_overlap_by_the_requested_width() {
        let lens = vec![1usize; 30];
        let p = plan(&lens, 10, 4);
        assert!(p.windows.len() > 1, "input is long enough to split");
        for pair in p.windows.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert_eq!(b.start, a.end - 4, "{a:?} -> {b:?} overlap by 4 words");
        }
    }

    #[test]
    fn an_entity_spanning_a_cut_is_whole_in_some_window() {
        // Any run of `width` consecutive words must sit entirely inside
        // at least one window, which is what the overlap buys.
        let lens = vec![1usize; 40];
        let width = 4;
        let p = plan(&lens, 9, width);
        for first in 0..=(lens.len() - width) {
            let last = first + width - 1;
            assert!(
                p.windows.iter().any(|w| w.start <= first && last < w.end),
                "no window wholly contains words {first}..={last}"
            );
        }
    }

    #[test]
    fn a_word_longer_than_a_window_is_reported_not_looped_on() {
        let p = plan(&[1, 99, 1], 10, 2);
        assert_eq!(
            p.oversized,
            vec![OversizedWord {
                index: 1,
                subtokens: 99
            }]
        );
        let placed: Vec<usize> = p
            .windows
            .iter()
            .flat_map(|w| (w.start..w.end).collect::<Vec<_>>())
            .collect();
        assert!(placed.contains(&0) && placed.contains(&2));
        assert!(!placed.contains(&1), "the oversized word is not placed");
    }

    #[test]
    fn an_overlap_wider_than_the_window_still_advances() {
        // A pathological overlap must not stall the walk.
        let lens = vec![1usize; 12];
        let p = plan(&lens, 3, 99);
        assert!(p.windows.len() < 50, "planner terminated");
        let last = p.windows.last().expect("at least one window");
        assert_eq!(last.end, lens.len(), "walk reached the end");
    }

    #[test]
    fn no_words_means_no_windows() {
        assert_eq!(
            plan(&[], 10, 2),
            Plan {
                windows: vec![],
                oversized: vec![]
            }
        );
    }
}
