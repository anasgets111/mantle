//! Edit-distance typo suggestions for config errors.

/// The candidate a mistyped `name` most likely meant, for a "did you mean" in a config error: the
/// nearest within two edits ignoring case, else the one candidate `name` begins. Names of three
/// characters or fewer get one edit, since two reach unrelated words (`top` to `gap`). Earlier
/// candidates win ties.
pub(crate) fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    // ponytail: no suggestion past 64 bytes (no key or choice is near that long); the edit table is O(len x candidate).
    const MAX_NAME_BYTES: usize = 64;
    if name.len() > MAX_NAME_BYTES {
        return None;
    }
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let limit = if name.len() <= 3 { 1 } else { 2 };
    let candidates: Vec<(&str, Vec<char>)> =
        candidates.into_iter().map(|candidate| (candidate, candidate.to_lowercase().chars().collect())).collect();
    let nearest = candidates
        .iter()
        .map(|(candidate, folded)| (edit_distance(&name, folded), *candidate))
        .filter(|&(distance, _)| distance <= limit)
        .min_by_key(|&(distance, _)| distance);
    if let Some((_, candidate)) = nearest {
        return Some(candidate);
    }
    let mut extending = candidates.iter().filter(|(_, folded)| name.len() >= 3 && folded.starts_with(&name));
    match (extending.next(), extending.next()) {
        (Some((candidate, _)), None) => Some(candidate),
        _ => None,
    }
}

/// The tail of a name error: "did you mean `near`?", else "expected one of `a`, `b`".
pub(crate) fn hint(name: &str, candidates: &[&str]) -> String {
    match closest(name, candidates.iter().copied()) {
        Some(near) => format!("did you mean `{near}`?"),
        None => {
            format!("expected one of {}", candidates.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", "))
        }
    }
}

/// Optimal string alignment distance: Levenshtein plus one-edit adjacent transpositions, the
/// commonest typo (`contnet`).
fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut table = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in table.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in table[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let substitution = table[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let mut best = substitution.min(table[i - 1][j] + 1).min(table[i][j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(table[i - 2][j - 2] + 1);
            }
            table[i][j] = best;
        }
    }
    table[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_takes_a_near_miss_or_a_unique_start_and_nothing_else() {
        let names = ["content", "font_size", "font_family", "gap", "width"];
        for (typo, meant) in [
            ("contnet", Some("content")), // transposition, one edit
            ("Width", Some("width")),     // case only
            ("fnot_size", Some("font_size")),
            ("fonts", None), // two starts, and two edits from neither
            ("font_fam", Some("font_family")),
            ("top", None), // two edits from `gap`, past a short name's one
            ("gao", Some("gap")),
            ("colour", None),
        ] {
            assert_eq!(closest(typo, names), meant, "{typo}");
        }
        // 64 bytes is the cap: one `a` more is one edit from the candidate, and still gets no suggestion.
        let at_cap = "a".repeat(64);
        assert_eq!(closest(&at_cap, [at_cap.as_str()]), Some(at_cap.as_str()));
        assert_eq!(closest(&format!("{at_cap}a"), [at_cap.as_str()]), None);
    }
}
