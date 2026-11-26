//! The order, search and letters of a game library, shared by the SteamOS
//! app and the iOS core.

/// The order a library shows its names in: all of them, or those that match
/// the search, best first.
pub fn library_view(names: &[&str], search: &str) -> Vec<usize> {
    use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::{CaseMatching, Normalization, Pattern}};
    if search.trim().is_empty() { return (0..names.len()).collect(); }
    let pattern = Pattern::parse(search, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, usize)> = names.iter().enumerate()
        .filter_map(|(index, name)| pattern.score(Utf32Str::new(name, &mut buffer), &mut matcher).map(|score| (score, index)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, index)| index).collect()
}

/// The letter a name is filed under: its first letter, or '#' for a name
/// that starts with a digit or a sign.
pub fn initial(name: &str) -> char {
    name.chars().find(|c| c.is_alphanumeric())
        .filter(|c| c.is_alphabetic())
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('#')
}

/// The order of names in a library: by the letter they are filed under,
/// then by name, so each letter of the rail is one run of names.
pub fn library_order(name: &str) -> (char, String) {
    (initial(name), name.to_lowercase())
}

/// The position of the first name of the next (`direction` 1) or previous
/// (-1) letter in `view`, or `position` when there is none.
pub fn letter_jump(names: &[&str], view: &[usize], position: usize, direction: isize) -> usize {
    let letter_at = |at: usize| view.get(at).map(|&index| initial(names[index]));
    let Some(current) = letter_at(position) else { return position };
    if direction > 0 {
        (position..view.len()).find(|&at| letter_at(at) != Some(current)).unwrap_or(position)
    } else {
        // The start of the group before the current one.
        let start = (0..=position).rev().take_while(|&at| letter_at(at) == Some(current)).last().unwrap_or(position);
        let Some(previous) = start.checked_sub(1).and_then(letter_at) else { return start };
        (0..start).find(|&at| letter_at(at) == Some(previous)).unwrap_or(start)
    }
}

/// The position of the first name filed under `letter` in `view`.
pub fn letter_position(names: &[&str], view: &[usize], letter: char) -> Option<usize> {
    view.iter().position(|&index| initial(names[index]) == letter)
}

/// The first letters of a name's first two words, for a tile without a cover.
pub fn monogram(name: &str) -> String {
    name.split_whitespace().filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2).flat_map(char::to_uppercase).collect()
}

/// A hue of its own for a name, in degrees, for a tile without a cover.
pub fn hue(name: &str) -> u16 {
    (name.bytes().fold(7u32, |hash, byte| hash.wrapping_mul(31).wrapping_add(byte.into())) % 360) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_finds_names_loosely_best_first() {
        let names = ["BALL x PIT", "Balatro", "Diablo® IV", "No Man's Sky", "Desktop"];
        assert_eq!(library_view(&names, ""), [0, 1, 2, 3, 4]);
        assert_eq!(library_view(&names, "diablo")[0], 2);
        assert_eq!(library_view(&names, "nms")[0], 3);
        assert!(library_view(&names, "zzz").is_empty());
    }

    #[test]
    fn names_are_filed_under_their_first_letter() {
        assert_eq!(initial("ball x pit"), 'B');
        assert_eq!(initial("“Nodebuster”"), 'N');
        assert_eq!(initial("7 Days to Die"), '#');
        assert_eq!(monogram("No Man's Sky"), "NM");
        assert_eq!(monogram("Desktop"), "D");
    }

    #[test]
    fn the_triggers_jump_between_letters() {
        let names = ["Balatro", "BALL x PIT", "Cyberpunk 2077", "Desktop", "Diablo IV"];
        let view: Vec<usize> = (0..names.len()).collect();
        assert_eq!(letter_jump(&names, &view, 0, 1), 2);
        assert_eq!(letter_jump(&names, &view, 2, 1), 3);
        assert_eq!(letter_jump(&names, &view, 4, 1), 4);
        // Back goes to the start of the letter before, from anywhere in a letter.
        assert_eq!(letter_jump(&names, &view, 4, -1), 2);
        assert_eq!(letter_jump(&names, &view, 2, -1), 0);
        assert_eq!(letter_jump(&names, &view, 1, -1), 0);
        assert_eq!(letter_position(&names, &view, 'D'), Some(3));
    }

    #[test]
    fn each_letter_is_one_run_of_names() {
        let mut names = vec!["!Zelda", "Apex", "Zork", "Élan", "apple"];
        names.sort_by_key(|name| library_order(name));
        assert_eq!(names, ["Apex", "apple", "!Zelda", "Zork", "Élan"]);
    }
}
