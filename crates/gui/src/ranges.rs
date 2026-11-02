//! `{name}` placeholders in field texts and the series of labels their ranges expand to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Kept as typed: the width of `start` sets the zero padding, and a half-typed step must survive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Range {
    pub start: String,
    pub stop: String,
    pub step: String,
}

impl Default for Range {
    fn default() -> Self {
        Self { start: "1".into(), stop: "1".into(), step: "1".into() }
    }
}

/// How several ranges combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every combination; the first name is the outer loop.
    #[default]
    Product,
    /// Side by side, as long as the shortest range.
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    Start,
    Stop,
    Step,
    ZeroStep,
    /// The step walks away from `stop`.
    Direction,
}

/// A valid range: `count` values from `start`, `step` apart, `width` digits at least.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Values {
    start: u64,
    step: i64,
    count: u64,
    width: usize,
}

impl Values {
    fn get(&self, i: u64) -> String {
        let v = self.start as i128 + i as i128 * self.step as i128;
        format!("{v:0width$}", width = self.width)
    }
}

fn number(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl Range {
    fn values(&self) -> Result<Values, RangeError> {
        let start = number(&self.start).ok_or(RangeError::Start)?;
        let stop = number(&self.stop).ok_or(RangeError::Stop)?;
        let step: i64 = self.step.trim().parse().map_err(|_| RangeError::Step)?;
        let span = stop.abs_diff(start);
        let count = match step {
            0 => return Err(RangeError::ZeroStep),
            _ if start == stop => 1,
            _ if (step > 0) != (stop > start) => return Err(RangeError::Direction),
            _ => span / step.unsigned_abs() + 1,
        };
        Ok(Values { start, step, count, width: self.start.trim().len() })
    }

    pub fn error(&self) -> Option<RangeError> {
        self.values().err()
    }
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Text(&'a str),
    Name(&'a str),
}

/// Anything in braces that is not a name stays text, braces included.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut text_from = 0;
    let mut i = 0;
    while let Some(open) = text[i..].find('{').map(|o| i + o) {
        let Some(close) = text[open + 1..].find('}').map(|c| open + 1 + c) else { break };
        let inner = &text[open + 1..close];
        if is_name(inner) {
            if text_from < open {
                out.push(Piece::Text(&text[text_from..open]));
            }
            out.push(Piece::Name(inner));
            text_from = close + 1;
            i = close + 1;
        } else {
            i = open + 1;
        }
    }
    if text_from < text.len() {
        out.push(Piece::Text(&text[text_from..]));
    }
    out
}

/// Every name used in `texts`, in order of first appearance.
pub fn names<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        for piece in pieces(text) {
            if let Piece::Name(n) = piece
                && !out.iter().any(|o| o == n)
            {
                out.push(n.to_owned());
            }
        }
    }
    out
}

/// The labels to print: one text per field for each of `count` labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    names: Vec<String>,
    values: Vec<Values>,
    mode: Mode,
    pub count: u64,
}

impl Series {
    /// `None` while any range is invalid; a name without a range gets the default one.
    pub fn new(names: &[String], ranges: &BTreeMap<String, Range>, mode: Mode) -> Option<Self> {
        let values = names
            .iter()
            .map(|n| ranges.get(n).cloned().unwrap_or_default().values())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let counts = values.iter().map(|v| v.count);
        let count = match mode {
            Mode::Product => counts.fold(1u64, u64::saturating_mul),
            Mode::Zip => counts.min().unwrap_or(1),
        };
        Some(Self { names: names.to_vec(), values, mode, count })
    }

    /// Label `i`: the name → value pairs, the last name turning fastest.
    fn vars(&self, mut i: u64) -> Vec<(&str, String)> {
        let mut out = Vec::with_capacity(self.names.len());
        for (name, v) in self.names.iter().zip(&self.values).rev() {
            let at = match self.mode {
                Mode::Product => {
                    let at = i % v.count;
                    i /= v.count;
                    at
                }
                Mode::Zip => i,
            };
            out.push((name.as_str(), v.get(at)));
        }
        out
    }

    /// `texts` with every name replaced by its value on label `i`.
    pub fn texts(&self, texts: &[String], i: u64) -> Vec<String> {
        let vars = self.vars(i);
        texts
            .iter()
            .map(|text| {
                pieces(text)
                    .into_iter()
                    .map(|p| match p {
                        Piece::Text(s) => s,
                        Piece::Name(n) => vars.iter().find(|(name, _)| *name == n).map_or(n, |(_, v)| v.as_str()),
                    })
                    .collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: &str, stop: &str, step: &str) -> Range {
        Range { start: start.into(), stop: stop.into(), step: step.into() }
    }

    fn all(r: Range) -> Vec<String> {
        let v = r.values().unwrap();
        (0..v.count).map(|i| v.get(i)).collect()
    }

    #[test]
    fn stop_is_included_and_never_overshot() {
        assert_eq!(all(range("1", "3", "1")), ["1", "2", "3"]);
        assert_eq!(all(range("0", "10", "3")), ["0", "3", "6", "9"]);
        assert_eq!(all(range("5", "5", "-2")), ["5"]);
    }

    #[test]
    fn negative_step_counts_down() {
        assert_eq!(all(range("10", "0", "-4")), ["10", "06", "02"]);
        assert_eq!(all(range("9", "11", "1")), ["9", "10", "11"]);
    }

    #[test]
    fn start_sets_the_zero_padding() {
        assert_eq!(all(range("000", "002", "1")), ["000", "001", "002"]);
        assert_eq!(all(range("0", "100", "50")), ["0", "50", "100"]);
        assert_eq!(all(range("998", "1000", "1")), ["998", "999", "1000"]);
        assert_eq!(all(range("010", "000", "-5")), ["010", "005", "000"]);
    }

    #[test]
    fn bad_ranges_are_flagged() {
        assert_eq!(range("", "3", "1").error(), Some(RangeError::Start));
        assert_eq!(range("-1", "3", "1").error(), Some(RangeError::Start));
        assert_eq!(range("1", "x", "1").error(), Some(RangeError::Stop));
        assert_eq!(range("1", "3", "-").error(), Some(RangeError::Step));
        assert_eq!(range("1", "3", "0").error(), Some(RangeError::ZeroStep));
        assert_eq!(range("10", "0", "1").error(), Some(RangeError::Direction));
        assert_eq!(range("0", "10", "-1").error(), Some(RangeError::Direction));
    }

    #[test]
    fn only_names_in_braces_are_placeholders() {
        assert_eq!(names(["Ауд. {n}, {}, {a b}, {", "{row_1}-{n}{x"]), ["n", "row_1"]);
        assert_eq!(pieces("{{n}}"), [Piece::Text("{"), Piece::Name("n"), Piece::Text("}")]);
    }

    fn series(mode: Mode) -> Series {
        let ranges = BTreeMap::from([("row".into(), range("1", "3", "1")), ("seat".into(), range("01", "02", "1"))]);
        Series::new(&["row".into(), "seat".into()], &ranges, mode).unwrap()
    }

    #[test]
    fn product_turns_the_last_name_fastest() {
        let s = series(Mode::Product);
        let texts = ["{row}-{seat}".to_owned(), "Ряд {row}".to_owned()];
        let labels: Vec<Vec<String>> = (0..s.count).map(|i| s.texts(&texts, i)).collect();
        assert_eq!(s.count, 6);
        assert_eq!(labels[0], ["1-01", "Ряд 1"]);
        assert_eq!(labels[1], ["1-02", "Ряд 1"]);
        assert_eq!(labels[5], ["3-02", "Ряд 3"]);
    }

    #[test]
    fn zip_stops_at_the_shortest() {
        let s = series(Mode::Zip);
        assert_eq!(s.count, 2);
        assert_eq!(s.texts(&["{row}/{seat}".into()], 1), ["2/02"]);
    }

    #[test]
    fn a_missing_range_is_one_label_and_a_bad_one_is_none() {
        let s = Series::new(&["n".into()], &BTreeMap::new(), Mode::Product).unwrap();
        assert_eq!((s.count, s.texts(&["#{n}".into()], 0)), (1, vec!["#1".to_owned()]));
        let bad = BTreeMap::from([("n".into(), range("1", "3", "0"))]);
        assert_eq!(Series::new(&["n".into()], &bad, Mode::Product), None);
    }
}
