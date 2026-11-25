//! Homebrew-flavoured version comparison.
//!
//! `1.10 > 1.9`, `2024.1 > 2024.1-rc1`-ish semantics, numeric segments compare
//! numerically, numeric beats alphabetic, and trailing zero segments are equal
//! (`1.0 == 1`).

use std::cmp::Ordering;

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    /// Digits only, without leading zeros (`""` == zero).
    Num(&'a str),
    Alpha(&'a str),
}

fn tokenize(s: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if b[i].is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &s[start..i];
            let trimmed = digits.trim_start_matches('0');
            out.push(Tok::Num(trimmed));
        } else if b[i].is_ascii_alphabetic() {
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            out.push(Tok::Alpha(&s[start..i]));
        } else {
            i += 1;
        }
    }
    out
}

fn cmp_num(a: &str, b: &str) -> Ordering {
    match a.len().cmp(&b.len()) {
        Ordering::Equal => a.cmp(b),
        other => other,
    }
}

fn is_zero(t: &Tok<'_>) -> bool {
    matches!(t, Tok::Num(n) if n.is_empty())
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let ta = tokenize(a);
    let tb = tokenize(b);
    let mut i = 0;
    loop {
        match (ta.get(i), tb.get(i)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => {
                return if tb[i..].iter().all(is_zero) {
                    Ordering::Equal
                } else if matches!(tb[i], Tok::Alpha(_)) {
                    // Missing side is an implicit numeric 0; 0 > "rc".
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (Some(_), None) => {
                return if ta[i..].iter().all(is_zero) {
                    Ordering::Equal
                } else if matches!(ta[i], Tok::Alpha(_)) {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (Some(x), Some(y)) => {
                let ord = match (x, y) {
                    (Tok::Num(p), Tok::Num(q)) => cmp_num(p, q),
                    (Tok::Num(_), Tok::Alpha(_)) => Ordering::Greater,
                    (Tok::Alpha(_), Tok::Num(_)) => Ordering::Less,
                    (Tok::Alpha(p), Tok::Alpha(q)) => p.cmp(q),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
        i += 1;
    }
}

/// The directory name a keg lives under: `1.7.1`, or `1.7.1_2` with a revision.
pub fn keg_dir_name(version: &str, revision: u32) -> String {
    if revision == 0 {
        version.to_string()
    } else {
        format!("{version}_{revision}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ordering() {
        assert_eq!(compare("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare("1.2.11", "1.2.3"), Ordering::Greater);
        assert_eq!(compare("2.0", "10.0"), Ordering::Less);
    }

    #[test]
    fn trailing_zeros_equal() {
        assert_eq!(compare("1.0", "1"), Ordering::Equal);
        assert_eq!(compare("1.0.0", "1"), Ordering::Equal);
        assert_eq!(compare("1.0.1", "1"), Ordering::Greater);
    }

    #[test]
    fn prerelease_sorts_before() {
        assert_eq!(compare("1.0.0", "1.0.0-rc1"), Ordering::Greater);
    }

    #[test]
    fn alpha_and_numeric_mix() {
        assert_eq!(compare("1.9a", "1.9b"), Ordering::Less);
        assert_eq!(compare("jq-1.7", "jq-1.7"), Ordering::Equal);
    }

    #[test]
    fn keg_names() {
        assert_eq!(keg_dir_name("1.7.1", 0), "1.7.1");
        assert_eq!(keg_dir_name("1.7.1", 2), "1.7.1_2");
    }
}
