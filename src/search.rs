use crate::layout::{Destination, Mapping};
use std::ops::Range;

#[derive(Default)]
pub struct Segment {
    pub text: String,
    pub mappings: Vec<Mapping>,
    pub destinations: Vec<Destination>,
}

#[derive(Clone, Debug)]
pub struct Match {
    pub ranges: Vec<Range<usize>>,
    pub destinations: Vec<(usize, Range<usize>)>,
    pub anchor: usize,
}
impl Match {
    pub fn start(&self) -> usize {
        self.anchor
    }
    pub fn contains(&self, source: &Range<usize>) -> bool {
        !source.is_empty()
            && self
                .ranges
                .iter()
                .any(|r| r.start < source.end && source.start < r.end)
    }
}

pub fn find(segments: &[Segment], query: &str) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    for segment in segments {
        for (start, text) in segment.text.match_indices(query) {
            let end = start + text.len();
            let first = segment.mappings.partition_point(|m| m.text.end <= start);
            let mut ranges: Vec<Range<usize>> = Vec::new();
            let mut destinations = Vec::new();
            let mut anchor = None;
            for Mapping {
                text: logical,
                source,
            } in segment.mappings[first..]
                .iter()
                .take_while(|m| m.text.start < end)
            {
                anchor.get_or_insert(source.start);
                let dest_first = segment
                    .destinations
                    .partition_point(|d| d.text.end <= logical.start);
                if let Some(d) = segment
                    .destinations
                    .get(dest_first)
                    .filter(|d| d.text.start < logical.end)
                {
                    destinations.push((d.id, d.target.clone()));
                    continue;
                }
                let range = if logical.len() == source.len() {
                    source.start + start.saturating_sub(logical.start)
                        ..source.end - logical.end.saturating_sub(end)
                } else {
                    source.clone()
                };
                if ranges.is_empty() && destinations.is_empty() {
                    anchor = Some(range.start);
                }
                if let Some(last) = ranges.last_mut().filter(|last| last.end == range.start) {
                    last.end = range.end;
                } else {
                    ranges.push(range);
                }
            }
            result.push(Match {
                ranges,
                destinations,
                anchor: anchor.unwrap_or(0),
            });
        }
    }
    result.sort_by_key(Match::start);
    result
}

#[derive(Default)]
pub struct Search {
    pub query: String,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    pub input: Option<String>,
    pub candidates: Vec<Match>,
    pub limited: bool,
    pub wrapped: bool,
    pub destination_matches: std::collections::HashMap<usize, Vec<(Range<usize>, usize)>>,
}

impl Search {
    pub fn index_destinations(&mut self) {
        self.destination_matches.clear();
        for (i, matched) in self.matches.iter().enumerate() {
            for (id, range) in &matched.destinations {
                let entries = self.destination_matches.entry(*id).or_default();
                if let Some((previous, previous_index)) = entries
                    .last_mut()
                    .filter(|(r, index)| *index == i && r.end == range.start)
                {
                    previous.end = range.end;
                    *previous_index = i;
                } else {
                    entries.push((range.clone(), i));
                }
            }
        }
        for entries in self.destination_matches.values_mut() {
            entries.sort_by_key(|(range, _)| range.start);
        }
    }
}
